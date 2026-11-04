//! One client connection, without I/O: bytes come in with [`Connection::receive`], whole
//! requests come out of [`Connection::next_request`], and [`Connection::reply`] turns a reply
//! document into the bytes to write (compressed like the request, with a checksum if it had
//! one, as an `OP_REPLY` for the `OP_QUERY` handshake).

use teseladb_bson::{Document, RawDoc, Value};

use crate::commands::auth::AuthState;
use crate::compressed::{self, Compressor};
use crate::error::CommandError;
use crate::failpoint::Fault;
use crate::msg::{HEADER_LEN, Header, OpMsg, command_limits, flags, opcode, reply_limits};
use crate::query::{AWAIT_CAPABLE, OpQuery, QUERY_FAILURE, encode_reply};
use crate::{MAX_MESSAGE_SIZE, WireError};

/// How a request arrived, which decides how it is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `OP_MSG`.
    Msg,
    /// `OP_QUERY` on `<db>.$cmd`: the handshake.
    Query,
    /// `OP_QUERY` on a collection, removed from RefDb.
    LegacyQuery,
}

/// A request read from the connection.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Its header (of the original message when it came compressed).
    pub header: Header,
    /// How it arrived.
    pub kind: Kind,
    /// The compressor it came with.
    pub compressor: Option<Compressor>,
    /// `OP_MSG` flag bits (0 for `OP_QUERY`).
    pub flags: u32,
    /// The command's database (`$db`, or the namespace of an `OP_QUERY`); empty if missing.
    pub db: String,
    /// The command: the body with the document sequences as array fields.
    pub command: Document,
    /// The fields of `command` that arrived as document sequences, which RefDb leaves out of
    /// the command it quotes in an `Unauthorized` message (013).
    pub sequences: Vec<String>,
    /// An error to answer instead of running it.
    pub problem: Option<CommandError>,
}

impl Request {
    /// The command's name: the body's first field.
    pub fn name(&self) -> &str {
        self.command.iter().next().map_or("", |(k, _)| k)
    }

    /// The client sends no more and expects no reply (`moreToCome`).
    pub fn more_to_come(&self) -> bool {
        self.flags & flags::MORE_TO_COME != 0
    }

    /// The client accepts a stream of replies (`exhaustAllowed`).
    pub fn exhaust_allowed(&self) -> bool {
        self.flags & flags::EXHAUST_ALLOWED != 0
    }
}

/// What the handshake keeps per connection.
#[derive(Clone, Debug, PartialEq)]
pub struct HelloState {
    /// Compressors the node takes (`--networkMessageCompressors`).
    pub enabled: Vec<Compressor>,
    /// Compressors negotiated in a `hello` (in the client's order).
    pub compressors: Vec<Compressor>,
    /// A `hello` was answered already: client metadata may not come again.
    pub greeted: bool,
    /// `client.application.name` of the first `hello`.
    pub app_name: Option<String>,
    /// The client metadata document of the first `hello`.
    pub metadata: Option<Document>,
}

impl Default for HelloState {
    fn default() -> HelloState {
        HelloState {
            enabled: Compressor::ALL.to_vec(),
            compressors: Vec::new(),
            greeted: false,
            app_name: None,
            metadata: None,
        }
    }
}

/// A client connection.
#[derive(Debug)]
pub struct Connection {
    id: i32,
    buf: Vec<u8>,
    next_id: i32,
    /// Handshake state.
    pub hello: HelloState,
    /// What the failpoint does to the request held by `blockConnection`, when it runs again.
    pub fault: Option<Fault>,
    /// The client's address (`host:port`), when the server knows it.
    pub peer: Option<String>,
    /// The client holds the action `bypassDefaultMaxTimeMS` on the cluster: set by each
    /// authorization; always on a node that does not authorize, as on a RefDb without `--auth`.
    pub bypass_default_max_time: bool,
    /// Its authentication (013).
    pub auth: AuthState,
    /// The tenant the request being run works in (007 FR-025).
    pub tenant: u32,
}

/// The longest message a connection takes before it authenticates, with authentication on: a
/// longer one closes it at its header, without an answer (measured on 04-11-2026 on a `refrouter`
/// and a `refnode` 9.0.4: 16,384 bytes answered, 16,385 closed).
pub const UNAUTHENTICATED_MAX_MESSAGE: usize = 16 * 1024;

impl Connection {
    /// A new connection with `connectionId` `id` and every compressor enabled.
    pub fn new(id: i32) -> Connection {
        Connection::with_compressors(id, &Compressor::ALL)
    }

    /// A new connection that takes only the compressors of `enabled`.
    pub fn with_compressors(id: i32, enabled: &[Compressor]) -> Connection {
        let hello = HelloState { enabled: enabled.to_vec(), ..HelloState::default() };
        Connection {
            id,
            buf: Vec::new(),
            next_id: 1,
            hello,
            fault: None,
            peer: None,
            bypass_default_max_time: true,
            auth: AuthState::default(),
            tenant: 0,
        }
    }

    /// Its `connectionId`.
    pub fn id(&self) -> i32 {
        self.id
    }

    /// Bytes read from the socket.
    pub fn receive(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received and not yet consumed by a request.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next whole request, if one arrived. An error means the connection must close.
    pub fn next_request(&mut self) -> Result<Option<Request>, WireError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = i32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let size = usize::try_from(len)
            .ok()
            .filter(|n| (HEADER_LEN..=MAX_MESSAGE_SIZE).contains(n))
            .ok_or(WireError::MessageLength(len))?;
        if size > UNAUTHENTICATED_MAX_MESSAGE && self.auth.required && self.auth.user.is_none() {
            return Err(WireError::UnauthenticatedLength(len));
        }
        if self.buf.len() < size {
            return Ok(None);
        }
        let whole: Vec<u8> = self.buf.drain(..size).collect();
        let header = Header::parse(&whole);
        match header.op_code {
            opcode::OP_MSG => msg_request(&whole, None).map(Some),
            opcode::OP_QUERY => query_request(&whole).map(Some),
            opcode::OP_COMPRESSED => {
                let (inner, c) = compressed::decompress(&whole)?;
                if !self.hello.enabled.contains(&c) {
                    return Err(WireError::UnknownCompressor(c.id()));
                }
                msg_request(&inner, Some(c)).map(Some)
            }
            op => Err(WireError::UnsupportedOpcode(op)),
        }
    }

    /// The bytes of the reply `doc` to `req` (none for a `moreToCome` request: the caller does
    /// not write them). `more_to_come` streams it (exhaust).
    pub fn reply(&mut self, req: &Request, doc: &Document, more_to_come: bool) -> Vec<u8> {
        self.reply_to(req.header.request_id, req, doc, more_to_come).0
    }

    /// [`Connection::reply`] answering message `response_to` (the previous reply of an exhaust
    /// stream); the bytes and the reply's request id.
    pub fn reply_to(
        &mut self,
        response_to: i32,
        req: &Request,
        doc: &Document,
        more_to_come: bool,
    ) -> (Vec<u8>, i32) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        // A reply the server built that does not encode (a name with a zero byte, or past the
        // limits) is answered with the error of that BSON fault.
        let body = doc.to_bytes_with(reply_limits()).unwrap_or_else(|e| {
            let (code, name) = e.code();
            let err = crate::error::Code { code, name };
            let fallback = CommandError::new(err, "reply document is invalid or too large");
            fallback.to_document().to_bytes().unwrap_or_default()
        });
        let bytes = match req.kind {
            Kind::Msg => {
                let mut bits = req.flags & flags::CHECKSUM_PRESENT;
                if more_to_come {
                    bits |= flags::MORE_TO_COME;
                }
                crate::msg::OpMsg::encode(id, response_to, bits, &body)
            }
            Kind::Query => encode_reply(id, response_to, AWAIT_CAPABLE, &body),
            Kind::LegacyQuery => encode_reply(id, response_to, QUERY_FAILURE, &body),
        };
        match req.compressor {
            Some(c) => (compressed::compress(&bytes, c), id),
            None => (bytes, id),
        }
    }
}

fn msg_request(whole: &[u8], compressor: Option<Compressor>) -> Result<Request, WireError> {
    let header = Header::parse(whole);
    let m = OpMsg::parse(whole)?;
    let mut command = match &m.body {
        Some(b) => {
            Document::from(RawDoc::from_bytes_with(b, command_limits()).map_err(WireError::Bson)?)
        }
        None => Document::new(),
    };
    let problem = m.problem;
    let mut sequences = Vec::with_capacity(m.sequences.len());
    for s in m.sequences {
        if command.get(&s.id).is_some() {
            // A `refrouter` 9.0 closes the connection (measured).
            return Err(WireError::Malformed("Duplicate field between body and document sequence"));
        }
        let mut docs = Vec::with_capacity(s.docs.len());
        for d in &s.docs {
            let raw = RawDoc::from_bytes_with(d, command_limits()).map_err(WireError::Bson)?;
            docs.push(Value::Document(Document::from(raw)));
        }
        sequences.push(s.id.clone());
        command.push(s.id, docs);
    }
    let db = match command.get("$db") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    Ok(Request {
        header,
        kind: Kind::Msg,
        compressor,
        flags: m.flags,
        db,
        command,
        sequences,
        problem,
    })
}

fn query_request(whole: &[u8]) -> Result<Request, WireError> {
    let header = Header::parse(whole);
    let q = OpQuery::parse(whole)?;
    let raw = RawDoc::from_bytes_with(&q.query, command_limits()).map_err(WireError::Bson)?;
    let (kind, db) = match q.command_db() {
        Some(db) => (Kind::Query, db.to_owned()),
        None => (Kind::LegacyQuery, String::new()),
    };
    let command = Document::from(raw);
    Ok(Request {
        header,
        kind,
        compressor: None,
        flags: 0,
        db,
        command,
        sequences: Vec::new(),
        problem: None,
    })
}
