//! The message header and `OP_MSG` (contract `wire-messages.md`, FR-001).
//!
//! A structural fault (a length that does not add up, an unknown required flag, a bad checksum,
//! invalid BSON) closes the connection, as in RefDb; a fault RefDb answers with an error
//! reply (two bodies, no body, a security token section, a document nested too deep) comes back
//! as [`OpMsg::problem`].
//!
//! The header is read with little-endian integer reads. Each document is checked in one pass
//! ([`read_message`], 006 FR-039) as a `refrouter` 9.0.4 checks it (the first fault in document
//! order decides between closing and answering) and with the rules of the BSON specification
//! RefDb does not apply: a string or name that is not UTF-8 and an old binary whose two lengths
//! differ get an `InvalidBSON` reply where RefDb would store them (003 T009). A BSON column that
//! does not follow the column layout gets RefDb's `NonConformantBSON` (003 T029). A canonical
//! document keeps the bytes it came in: its fields are read only when the command asks for them,
//! and a stored one is written by copying those bytes.

use teseladb_bson::{BsonError, Document, Fragments, Limits, RawDoc, first_element, read_message};
use teseladb_hash::{Crc32c, crc32c};

use crate::WireError;
use crate::error::{
    CommandError, DUPLICATE_SEQUENCE, NON_CONFORMANT_BSON, OVERFLOW, UNAUTHORIZED, UNKNOWN_SECTION,
};

/// Length of the header.
pub const HEADER_LEN: usize = 16;

/// Operation codes.
pub mod opcode {
    /// Reply to an `OP_QUERY`.
    pub const OP_REPLY: i32 = 1;
    /// Legacy query, only for the handshake.
    pub const OP_QUERY: i32 = 2004;
    /// Compressed envelope.
    pub const OP_COMPRESSED: i32 = 2012;
    /// Extensible message.
    pub const OP_MSG: i32 = 2013;
}

/// `OP_MSG` flag bits.
pub mod flags {
    /// The message ends with a CRC-32C.
    pub const CHECKSUM_PRESENT: u32 = 1;
    /// The sender will not wait for a reply / more replies follow.
    pub const MORE_TO_COME: u32 = 1 << 1;
    /// The client accepts several replies to this request.
    pub const EXHAUST_ALLOWED: u32 = 1 << 16;
    /// Bits 0 to 15: a receiver must know every one that is set.
    pub(crate) const REQUIRED: u32 = 0xffff;
}

/// Section kinds.
const KIND_BODY: u8 = 0;
const KIND_SEQUENCE: u8 = 1;
const KIND_SECURITY_TOKEN: u8 = 2;

/// Command documents may exceed a stored document by 16 KiB, as in RefDb.
pub const MAX_COMMAND_SIZE: usize = teseladb_bson::MAX_SIZE + 16 * 1024;

pub use teseladb_bson::MAX_COMMAND_DEPTH;

/// Levels a reply adds around the documents it carries (`cursor.firstBatch.<i>`, and one spare).
const REPLY_EXTRA_DEPTH: usize = 4;

/// Limits of the documents of a command.
pub(crate) fn command_limits() -> Limits {
    Limits { max_size: MAX_COMMAND_SIZE, max_depth: MAX_COMMAND_DEPTH }
}

/// Limits of a reply document: a command's size, and room for the levels around the documents
/// of a command it carries back.
pub(crate) fn reply_limits() -> Limits {
    Limits { max_size: MAX_COMMAND_SIZE, max_depth: MAX_COMMAND_DEPTH + REPLY_EXTRA_DEPTH }
}

/// The header of every message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Total length, header included.
    pub length: i32,
    /// Identifier the sender gives the message.
    pub request_id: i32,
    /// The request this one answers.
    pub response_to: i32,
    /// Operation code.
    pub op_code: i32,
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Header {
    /// Reads the first [`HEADER_LEN`] bytes of `b` (which must have them).
    pub fn parse(b: &[u8]) -> Header {
        Header {
            length: i32_at(b, 0),
            request_id: i32_at(b, 4),
            response_to: i32_at(b, 8),
            op_code: i32_at(b, 12),
        }
    }

    /// Appends the header to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        for v in [self.length, self.request_id, self.response_to, self.op_code] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// A document sequence (section kind 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    /// Identifier: the name of the field it stands for.
    pub id: String,
    /// Its documents.
    pub docs: Vec<Document>,
}

/// A parsed `OP_MSG`.
#[derive(Clone, Debug, PartialEq)]
pub struct OpMsg {
    /// Flag bits.
    pub flags: u32,
    /// The body (section kind 0), if there was one.
    pub body: Option<Document>,
    /// Document sequences.
    pub sequences: Vec<Sequence>,
    /// An error RefDb answers instead of running the command.
    pub problem: Option<CommandError>,
}

fn cstring(b: &[u8], at: usize) -> Result<(&str, usize), WireError> {
    let len = teseladb_bson::find_nul(&b[at..]).ok_or(WireError::Malformed("unterminated string"))?;
    let s =
        std::str::from_utf8(&b[at..at + len]).map_err(|_| WireError::Malformed("invalid UTF-8"))?;
    Ok((s, at + len + 1))
}

/// The bytes of the BSON document at the start of `b`, as long as its length says.
fn framed(b: &[u8]) -> Result<&[u8], WireError> {
    if b.len() < 5 {
        return Err(WireError::Malformed("document cut short"));
    }
    let len = i32_at(b, 0);
    usize::try_from(len)
        .ok()
        .and_then(|n| b.get(..n))
        .ok_or(WireError::Malformed("document length past the message"))
}

/// The BSON document at the start of `b`, validated as a command document; its bytes.
pub(crate) fn document(b: &[u8]) -> Result<&[u8], WireError> {
    let doc = framed(b)?;
    RawDoc::from_bytes_with(doc, command_limits()).map_err(WireError::Bson)?;
    Ok(doc)
}

/// A document of an `OP_MSG` section at the start of `b`: its length and the document, or the
/// error RefDb answers for it instead of closing the connection.
fn section_document(b: &[u8]) -> Result<Result<(usize, Document), CommandError>, WireError> {
    let doc = framed(b)?;
    match read_message(doc, command_limits()) {
        Ok(read) => Ok(read.map(|d| (doc.len(), d)).map_err(CommandError::from)),
        Err(BsonError::TooDeep { path, .. }) => Ok(Err(too_deep(doc, &path))),
        Err(BsonError::InvalidColumn { path, .. }) => Ok(Err(invalid_column(doc, &path))),
        Err(e) => Err(WireError::Bson(e)),
    }
}

/// How RefDb names a document of a message in its errors: by its `_id` when that is its first
/// field, with the quotes and backslashes of the value escaped (a `refrouter` 9.0.4 writes
/// `_id: \"k\"`, measured).
fn named(doc: &[u8]) -> String {
    match first_element(doc, command_limits()) {
        Some(("_id", v)) => {
            let shown = teseladb_mql::shell(v).replace('\\', "\\\\").replace('"', "\\\"");
            format!("_id: {shown}")
        }
        _ => "unknown _id".to_owned(),
    }
}

/// RefDb's `Overflow` for a document nested too deep at `path`.
fn too_deep(doc: &[u8], path: &str) -> CommandError {
    let text = format!(
        "BSONObj exceeds maximum nested object depth in element with field name '{path}' in \
         object with {}",
        named(doc)
    );
    CommandError::new(OVERFLOW, text)
}

/// RefDb's `NonConformantBSON` for a BSON column at `path` that does not follow the column
/// layout (measured on a `refrouter` 9.0.4, 003 T029).
fn invalid_column(doc: &[u8], path: &str) -> CommandError {
    let text = format!(
        "Invalid BSON column in element with field name '{path}' in object with {}",
        named(doc)
    );
    CommandError::new(NON_CONFORMANT_BSON, text)
}

impl OpMsg {
    /// Parses the message after its header (`whole` is the message with its header, for the
    /// checksum).
    pub fn parse(whole: &[u8]) -> Result<OpMsg, WireError> {
        let b = &whole[HEADER_LEN..];
        if b.len() < 4 {
            return Err(WireError::Malformed("OP_MSG without flags"));
        }
        let flag_bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let unknown =
            flag_bits & flags::REQUIRED & !(flags::CHECKSUM_PRESENT | flags::MORE_TO_COME);
        if unknown != 0 {
            return Err(WireError::IllegalFlags(flag_bits));
        }
        let mut end = b.len();
        if flag_bits & flags::CHECKSUM_PRESENT != 0 {
            if b.len() < 8 {
                return Err(WireError::Malformed("checksum missing"));
            }
            end -= 4;
            let want = u32::from_le_bytes([b[end], b[end + 1], b[end + 2], b[end + 3]]);
            if crc32c(&whole[..HEADER_LEN + end]) != want {
                return Err(WireError::Checksum);
            }
        }
        let mut msg = OpMsg { flags: flag_bits, body: None, sequences: Vec::new(), problem: None };
        let mut at = 4;
        while at < end {
            let kind = b[at];
            at += 1;
            match kind {
                KIND_BODY if msg.body.is_some() => {
                    // A `refrouter` 9.0 closes the connection (measured).
                    return Err(WireError::Malformed("Multiple body sections in message"));
                }
                KIND_BODY => match section_document(&b[at..end])? {
                    Ok((len, doc)) => {
                        at += len;
                        msg.body = Some(doc);
                    }
                    Err(problem) => {
                        msg.problem = Some(problem);
                        return Ok(msg);
                    }
                },
                KIND_SEQUENCE => match msg.sequence(&b[..end], at)? {
                    Ok(next) => at = next,
                    Err(problem) => {
                        msg.problem = Some(problem);
                        return Ok(msg);
                    }
                },
                KIND_SECURITY_TOKEN => {
                    msg.problem = Some(CommandError::new(
                        UNAUTHORIZED,
                        "Unsupported Security Token provided",
                    ));
                    return Ok(msg);
                }
                k => {
                    let text = format!("Unknown section kind {k}");
                    msg.problem = Some(CommandError::new(UNKNOWN_SECTION, text));
                    return Ok(msg);
                }
            }
        }
        if msg.body.is_none() {
            return Err(WireError::Malformed("OP_MSG messages must have a body"));
        }
        Ok(msg)
    }

    /// Reads a document sequence starting at `at` (after its kind byte); where it ends, or the
    /// error RefDb answers for one of its documents.
    fn sequence(&mut self, b: &[u8], at: usize) -> Result<Result<usize, CommandError>, WireError> {
        if b.len() < at + 4 {
            return Err(WireError::Malformed("document sequence cut short"));
        }
        let size =
            usize::try_from(i32_at(b, at)).map_err(|_| WireError::Malformed("negative size"))?;
        let end = at.checked_add(size).filter(|&e| e <= b.len() && size >= 4);
        let end = end.ok_or(WireError::Malformed("document sequence past the message"))?;
        let (id, mut pos) = cstring(&b[..end], at + 4)?;
        let mut docs = Vec::new();
        while pos < end {
            let (len, doc) = match section_document(&b[pos..end])? {
                Ok(read) => read,
                Err(problem) => return Ok(Err(problem)),
            };
            pos += len;
            docs.push(doc);
        }
        if self.sequences.iter().any(|s| s.id == id) && self.problem.is_none() {
            let text = format!("Duplicate document sequence: {id}");
            self.problem = Some(CommandError::new(DUPLICATE_SEQUENCE, text));
        }
        self.sequences.push(Sequence { id: id.to_owned(), docs });
        Ok(Ok(end))
    }

    /// [`OpMsg::encode`] with the body written from `doc` within `limits`, in the same buffer
    /// as the header: the document is not written apart and copied again (H-009 of the native
    /// tests, a reply of a scan carries tens of kilobytes).
    ///
    /// # Errors
    ///
    /// The [`BsonError`](teseladb_bson::BsonError) of a document that does not encode.
    pub fn encode_document(
        request_id: i32,
        response_to: i32,
        flag_bits: u32,
        doc: &teseladb_bson::Document,
        limits: Limits,
    ) -> Result<Vec<u8>, teseladb_bson::BsonError> {
        let checksum = flag_bits & flags::CHECKSUM_PRESENT != 0;
        let tail = if checksum { 4 } else { 0 };
        let mut out = Vec::with_capacity(HEADER_LEN + 5 + doc.encoded_len() + tail);
        let op_code = opcode::OP_MSG;
        // The length goes in once the body is written.
        Header { length: 0, request_id, response_to, op_code }.write(&mut out);
        out.extend_from_slice(&flag_bits.to_le_bytes());
        out.push(KIND_BODY);
        doc.append_to(&mut out, limits)?;
        let length = i32::try_from(out.len() + tail).unwrap_or(i32::MAX);
        out[..4].copy_from_slice(&length.to_le_bytes());
        if checksum {
            let crc = crc32c(&out);
            out.extend_from_slice(&crc.to_le_bytes());
        }
        Ok(out)
    }

    /// [`OpMsg::encode_document`] as pieces for a vectored write (006 FR-040): the header, the
    /// flags and the reply's own bytes are written here, and every document of `doc` that keeps
    /// the bytes it was stored with goes by reference to its own buffer, without a copy.
    ///
    /// # Errors
    ///
    /// The [`BsonError`](teseladb_bson::BsonError) of a document that does not encode.
    pub fn encode_fragments<'a>(
        request_id: i32,
        response_to: i32,
        flag_bits: u32,
        doc: &'a Document,
        limits: Limits,
    ) -> Result<Fragments<'a>, teseladb_bson::BsonError> {
        let checksum = flag_bits & flags::CHECKSUM_PRESENT != 0;
        let tail = if checksum { 4 } else { 0 };
        let mut out = Fragments::new(1);
        let mut head = Vec::with_capacity(HEADER_LEN + 5);
        Header { length: 0, request_id, response_to, op_code: opcode::OP_MSG }.write(&mut head);
        head.extend_from_slice(&flag_bits.to_le_bytes());
        head.push(KIND_BODY);
        out.extend(&head);
        doc.append_fragments(&mut out, limits)?;
        let length = i32::try_from(out.len() + tail).unwrap_or(i32::MAX);
        out.overwrite(0, &length.to_le_bytes());
        if checksum {
            let mut crc = Crc32c::new();
            for piece in out.slices() {
                crc.update(piece);
            }
            out.extend(&crc.value().to_le_bytes());
        }
        Ok(out)
    }

    /// Encodes a message with one body: header, flags, body and, with
    /// [`flags::CHECKSUM_PRESENT`], its CRC-32C.
    pub fn encode(request_id: i32, response_to: i32, flag_bits: u32, body: &[u8]) -> Vec<u8> {
        let checksum = flag_bits & flags::CHECKSUM_PRESENT != 0;
        let length = HEADER_LEN + 5 + body.len() + if checksum { 4 } else { 0 };
        let mut out = Vec::with_capacity(length);
        let op_code = opcode::OP_MSG;
        Header { length: length as i32, request_id, response_to, op_code }.write(&mut out);
        out.extend_from_slice(&flag_bits.to_le_bytes());
        out.push(KIND_BODY);
        out.extend_from_slice(body);
        if checksum {
            let crc = crc32c(&out);
            out.extend_from_slice(&crc.to_le_bytes());
        }
        out
    }
}
