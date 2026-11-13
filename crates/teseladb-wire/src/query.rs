//! `OP_QUERY` and `OP_REPLY`: only the handshake of drivers that do not know the server yet
//! (`hello` and `isMaster` on `<db>.$cmd`, FR-001). Any other command answers
//! `UnsupportedOpQueryCommand` and a query on a collection answers the legacy failure reply, as
//! in RefDb 6.0 and later.

use crate::WireError;
use crate::msg::{HEADER_LEN, Header, document, opcode};

/// `OP_REPLY` flag of a failed query.
pub const QUERY_FAILURE: i32 = 1 << 1;
/// `OP_REPLY` flag RefDb always sets.
pub const AWAIT_CAPABLE: i32 = 1 << 3;

/// Suffix of a command namespace.
const COMMAND_NS: &str = ".$cmd";

/// A parsed `OP_QUERY`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpQuery {
    /// Flag bits.
    pub flags: i32,
    /// `<db>.<collection>`.
    pub namespace: String,
    /// The query document.
    pub query: Vec<u8>,
}

impl OpQuery {
    /// Parses the message after its header.
    pub fn parse(whole: &[u8]) -> Result<OpQuery, WireError> {
        let b = &whole[HEADER_LEN..];
        if b.len() < 4 {
            return Err(WireError::Malformed("OP_QUERY cut short"));
        }
        let flags = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let len =
            teseladb_bson::find_nul(&b[4..]).ok_or(WireError::Malformed("unterminated namespace"))?;
        let namespace = std::str::from_utf8(&b[4..4 + len])
            .map_err(|_| WireError::Malformed("invalid UTF-8"))?;
        // Skip and return counts, then the query (a projection may follow; it is ignored).
        let at = 4 + len + 1 + 8;
        let query = document(b.get(at..).ok_or(WireError::Malformed("OP_QUERY cut short"))?)?;
        Ok(OpQuery { flags, namespace: namespace.to_owned(), query: query.to_vec() })
    }

    /// The database of a command namespace (`admin` of `admin.$cmd`); `None` if the namespace
    /// is not a command's.
    pub fn command_db(&self) -> Option<&str> {
        self.namespace.strip_suffix(COMMAND_NS)
    }
}

/// Encodes an `OP_REPLY` with one document.
pub fn encode_reply(request_id: i32, response_to: i32, flags: i32, doc: &[u8]) -> Vec<u8> {
    let length = HEADER_LEN + 20 + doc.len();
    let mut out = Vec::with_capacity(length);
    let op_code = opcode::OP_REPLY;
    Header { length: length as i32, request_id, response_to, op_code }.write(&mut out);
    out.extend_from_slice(&flags.to_le_bytes());
    // Cursor id 0, starting from 0, one document.
    out.extend_from_slice(&0i64.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&1i32.to_le_bytes());
    out.extend_from_slice(doc);
    out
}
