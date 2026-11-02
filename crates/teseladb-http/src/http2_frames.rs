//! The vocabulary of HTTP/2 frames (RFC 9113, sections 6 and 7) and the reading of a request
//! from its fields (section 8), for [`crate::http2`].

use crate::hpack::Field;
use crate::http1::Request;

/// Frame types (RFC 9113, section 6).
pub mod kind {
    /// DATA.
    pub const DATA: u8 = 0x0;
    /// HEADERS.
    pub const HEADERS: u8 = 0x1;
    /// PRIORITY.
    pub const PRIORITY: u8 = 0x2;
    /// RST_STREAM.
    pub const RST_STREAM: u8 = 0x3;
    /// SETTINGS.
    pub const SETTINGS: u8 = 0x4;
    /// PUSH_PROMISE.
    pub const PUSH_PROMISE: u8 = 0x5;
    /// PING.
    pub const PING: u8 = 0x6;
    /// GOAWAY.
    pub const GOAWAY: u8 = 0x7;
    /// WINDOW_UPDATE.
    pub const WINDOW_UPDATE: u8 = 0x8;
    /// CONTINUATION.
    pub const CONTINUATION: u8 = 0x9;
}

/// Frame flags.
pub mod flag {
    /// END_STREAM of DATA and HEADERS.
    pub const END_STREAM: u8 = 0x1;
    /// ACK of SETTINGS and PING.
    pub const ACK: u8 = 0x1;
    /// END_HEADERS of HEADERS and CONTINUATION.
    pub const END_HEADERS: u8 = 0x4;
    /// PADDED of DATA and HEADERS.
    pub const PADDED: u8 = 0x8;
    /// PRIORITY of HEADERS.
    pub const PRIORITY: u8 = 0x20;
}

/// Settings identifiers (RFC 9113, section 6.5.2).
pub mod setting {
    /// SETTINGS_HEADER_TABLE_SIZE.
    pub const HEADER_TABLE_SIZE: u16 = 0x1;
    /// SETTINGS_ENABLE_PUSH.
    pub const ENABLE_PUSH: u16 = 0x2;
    /// SETTINGS_MAX_CONCURRENT_STREAMS.
    pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    /// SETTINGS_INITIAL_WINDOW_SIZE.
    pub const INITIAL_WINDOW_SIZE: u16 = 0x4;
    /// SETTINGS_MAX_FRAME_SIZE.
    pub const MAX_FRAME_SIZE: u16 = 0x5;
    /// SETTINGS_MAX_HEADER_LIST_SIZE.
    pub const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// Error codes (RFC 9113, section 7).
pub mod code {
    /// NO_ERROR.
    pub const NO_ERROR: u32 = 0x0;
    /// PROTOCOL_ERROR.
    pub const PROTOCOL_ERROR: u32 = 0x1;
    /// FLOW_CONTROL_ERROR.
    pub const FLOW_CONTROL_ERROR: u32 = 0x3;
    /// STREAM_CLOSED.
    pub const STREAM_CLOSED: u32 = 0x5;
    /// FRAME_SIZE_ERROR.
    pub const FRAME_SIZE_ERROR: u32 = 0x6;
    /// REFUSED_STREAM.
    pub const REFUSED_STREAM: u32 = 0x7;
    /// CANCEL.
    pub const CANCEL: u32 = 0x8;
    /// COMPRESSION_ERROR.
    pub const COMPRESSION_ERROR: u32 = 0x9;
}

/// Names a request in HTTP/2 must not carry (RFC 9113, section 8.2.2).
pub(crate) const CONNECTION_FIELDS: [&str; 5] =
    ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade"];

/// The request of a header block, and the length its `content-length` announces; `Err` for a
/// malformed one (RFC 9113, section 8.1.1).
pub(crate) fn request_of(fields: &[Field]) -> Result<(Request, Option<usize>), ()> {
    let (mut method, mut scheme, mut path, mut authority) = (None, None, None, None);
    let mut headers = Vec::new();
    let mut regular = false;
    let mut length = None;
    for f in fields {
        let name = std::str::from_utf8(&f.name).map_err(|_| ())?;
        let value = std::str::from_utf8(&f.value).map_err(|_| ())?;
        if name.bytes().any(|b| b.is_ascii_uppercase() || b <= b' ' || b == 0x7f)
            || value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
        {
            return Err(());
        }
        if let Some(pseudo) = name.strip_prefix(':') {
            let slot = match pseudo {
                "method" => &mut method,
                "scheme" => &mut scheme,
                "path" => &mut path,
                "authority" => &mut authority,
                _ => return Err(()),
            };
            if regular || slot.is_some() {
                return Err(());
            }
            *slot = Some(value.to_owned());
            continue;
        }
        regular = true;
        if CONNECTION_FIELDS.contains(&name) || (name == "te" && value != "trailers") {
            return Err(());
        }
        if name == "content-length" {
            let n: usize = value.parse().map_err(|_| ())?;
            if length.is_some_and(|l| l != n) {
                return Err(());
            }
            length = Some(n);
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    let (Some(method), Some(_), Some(path)) = (method, scheme, path) else { return Err(()) };
    if path.is_empty() {
        return Err(());
    }
    if let Some(a) = authority
        && !headers.iter().any(|(n, _)| n == "host")
    {
        headers.insert(0, ("host".to_owned(), a));
    }
    Ok((Request { method, target: path, headers, body: Vec::new() }, length))
}
