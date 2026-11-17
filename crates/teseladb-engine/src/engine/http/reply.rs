//! HTTP replies: the [`Reply`] a handler fills and the client receives.
//!
//! A [`Reply`] carries the status, version, headers and either an in-memory body or a
//! [`BodyWriter`] that streams the body; the server writes it to the connection on the
//! shard that serves the request, with chunked transfer coding when the body is streamed.
//!
//! [`StatusType`] is a transparent newtype over `i32` with one associated constant per
//! known status code: the response parser builds it from any integer it reads, and a Rust
//! `enum` could not hold the codes that have no constant.

use std::collections::HashMap;
use std::fmt;

use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::iostream::OutputStream;
use crate::engine::core::loop_::do_for_each;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::http::internal::make_http_chunked_output_stream;
use crate::engine::http::httpd::httpd::Connection;
use crate::engine::http::mime_types::mime_types;
use crate::engine::util::iostream::util::write_to_stream_and_close;
use crate::engine::util::noncopyable_function::NoncopyableFunction;
use crate::engine::util::string_utils::internal::{
    CaseInsensitiveBuildHasher, CaseInsensitiveKey, CaseInsensitiveStr,
};

/// The status of the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct StatusType(pub i32);

impl StatusType {
    /// continue
    pub const CONTINUE: StatusType = StatusType(100);
    /// switching_protocols
    pub const SWITCHING_PROTOCOLS: StatusType = StatusType(101);
    /// ok
    pub const OK: StatusType = StatusType(200);
    /// created
    pub const CREATED: StatusType = StatusType(201);
    /// accepted
    pub const ACCEPTED: StatusType = StatusType(202);
    /// nonauthoritative_information
    pub const NONAUTHORITATIVE_INFORMATION: StatusType = StatusType(203);
    /// no_content
    pub const NO_CONTENT: StatusType = StatusType(204);
    /// reset_content
    pub const RESET_CONTENT: StatusType = StatusType(205);
    /// partial_content
    pub const PARTIAL_CONTENT: StatusType = StatusType(206);
    /// multiple_choices
    pub const MULTIPLE_CHOICES: StatusType = StatusType(300);
    /// moved_permanently
    pub const MOVED_PERMANENTLY: StatusType = StatusType(301);
    /// moved_temporarily
    pub const MOVED_TEMPORARILY: StatusType = StatusType(302);
    /// see_other
    pub const SEE_OTHER: StatusType = StatusType(303);
    /// not_modified
    pub const NOT_MODIFIED: StatusType = StatusType(304);
    /// use_proxy
    pub const USE_PROXY: StatusType = StatusType(305);
    /// temporary_redirect
    pub const TEMPORARY_REDIRECT: StatusType = StatusType(307);
    /// permanent_redirect
    pub const PERMANENT_REDIRECT: StatusType = StatusType(308);
    /// bad_request
    pub const BAD_REQUEST: StatusType = StatusType(400);
    /// unauthorized
    pub const UNAUTHORIZED: StatusType = StatusType(401);
    /// payment_required
    pub const PAYMENT_REQUIRED: StatusType = StatusType(402);
    /// forbidden
    pub const FORBIDDEN: StatusType = StatusType(403);
    /// not_found
    pub const NOT_FOUND: StatusType = StatusType(404);
    /// method_not_allowed
    pub const METHOD_NOT_ALLOWED: StatusType = StatusType(405);
    /// not_acceptable
    pub const NOT_ACCEPTABLE: StatusType = StatusType(406);
    /// request_timeout
    pub const REQUEST_TIMEOUT: StatusType = StatusType(408);
    /// conflict
    pub const CONFLICT: StatusType = StatusType(409);
    /// gone
    pub const GONE: StatusType = StatusType(410);
    /// length_required
    pub const LENGTH_REQUIRED: StatusType = StatusType(411);
    /// payload_too_large
    pub const PAYLOAD_TOO_LARGE: StatusType = StatusType(413);
    /// uri_too_long
    pub const URI_TOO_LONG: StatusType = StatusType(414);
    /// unsupported_media_type
    pub const UNSUPPORTED_MEDIA_TYPE: StatusType = StatusType(415);
    /// expectation_failed
    pub const EXPECTATION_FAILED: StatusType = StatusType(417);
    /// page_expired
    pub const PAGE_EXPIRED: StatusType = StatusType(419);
    /// unprocessable_entity
    pub const UNPROCESSABLE_ENTITY: StatusType = StatusType(422);
    /// upgrade_required
    pub const UPGRADE_REQUIRED: StatusType = StatusType(426);
    /// too_many_requests
    pub const TOO_MANY_REQUESTS: StatusType = StatusType(429);
    /// request_header_fields_too_large
    pub const REQUEST_HEADER_FIELDS_TOO_LARGE: StatusType = StatusType(431);
    /// login_timeout
    pub const LOGIN_TIMEOUT: StatusType = StatusType(440);
    /// internal_server_error
    pub const INTERNAL_SERVER_ERROR: StatusType = StatusType(500);
    /// not_implemented
    pub const NOT_IMPLEMENTED: StatusType = StatusType(501);
    /// bad_gateway
    pub const BAD_GATEWAY: StatusType = StatusType(502);
    /// service_unavailable
    pub const SERVICE_UNAVAILABLE: StatusType = StatusType(503);
    /// gateway_timeout
    pub const GATEWAY_TIMEOUT: StatusType = StatusType(504);
    /// http_version_not_supported
    pub const HTTP_VERSION_NOT_SUPPORTED: StatusType = StatusType(505);
    /// insufficient_storage
    pub const INSUFFICIENT_STORAGE: StatusType = StatusType(507);
    /// bandwidth_limit_exceeded
    pub const BANDWIDTH_LIMIT_EXCEEDED: StatusType = StatusType(509);
    /// network_read_timeout
    pub const NETWORK_READ_TIMEOUT: StatusType = StatusType(598);
    /// network_connect_timeout
    pub const NETWORK_CONNECT_TIMEOUT: StatusType = StatusType(599);
}

/// The classes of HTTP status codes, as listed at
/// <https://www.iana.org/assignments/http-status-codes/http-status-codes.xhtml>.
///
/// - 1xx, informational: the request was received, processing continues.
/// - 2xx, success: the request was received, understood and accepted.
/// - 3xx, redirection: further action is needed to complete the request.
/// - 4xx, client error: the request has bad syntax or cannot be fulfilled.
/// - 5xx, server error: the server failed to fulfil an apparently valid request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StatusClass {
    /// 1xx
    Informational = 1,
    /// 2xx
    Success = 2,
    /// 3xx
    Redirection = 3,
    /// 4xx
    ClientError = 4,
    /// 5xx
    ServerError = 5,
    /// Any other code.
    Unclassified,
}

/// The header map of requests and replies: keys hashed and compared case-insensitively.
pub type HeaderMap = HashMap<CaseInsensitiveKey, Sstring, CaseInsensitiveBuildHasher>;

/// The body writer of a reply: called once with the output stream, it writes the body and
/// resolves when done.
pub type BodyWriter = NoncopyableFunction<fn(OutputStream<u8>) -> Future<()>>;

/// An HTTP reply: one the server sends to a client, or one a client receives.
pub struct Reply {
    /// The status of the reply.
    pub _status: StatusType,
    /// The headers to be included in the reply.
    pub _headers: HeaderMap,
    /// The http version.
    pub _version: Sstring,
    /// The content to be sent in the reply.
    pub _content: Sstring,
    /// valid when received via client connection
    pub content_length: usize,
    /// The bytes of the body already consumed.
    pub consumed_content: usize,
    /// The response line, set by [`Reply::done`].
    pub _response_line: Sstring,
    /// The trailing headers of a chunked body.
    pub trailing_headers: HashMap<Sstring, Sstring>,
    /// The chunk extensions of a chunked body.
    pub chunk_extensions: HashMap<Sstring, Sstring>,
    pub(crate) _body_writer: BodyWriter,
    /// The reply answers a HEAD: everything but the body is written (RFC 7231 4.3.2).
    pub _skip_body: bool,
}

impl Default for Reply {
    fn default() -> Self {
        Self::new()
    }
}

impl Reply {
    /// Returns the class of the HTTP status `http_status`, from its hundreds digit; a
    /// status outside 100..=599 is [`StatusClass::Unclassified`].
    #[inline]
    pub const fn classify_status(http_status: StatusType) -> StatusClass {
        let sc = http_status.0 / 100;
        if sc < 1 || sc > 5 {
            return StatusClass::Unclassified;
        }
        match sc {
            1 => StatusClass::Informational,
            2 => StatusClass::Success,
            3 => StatusClass::Redirection,
            4 => StatusClass::ClientError,
            _ => StatusClass::ServerError,
        }
    }

    /// An empty reply with status ok.
    pub fn new() -> Self {
        Reply {
            _status: StatusType::OK,
            _headers: HeaderMap::default(),
            _version: Sstring::new(),
            _content: Sstring::new(),
            content_length: 0,
            consumed_content: 0,
            _response_line: Sstring::new(),
            trailing_headers: HashMap::new(),
            chunk_extensions: HashMap::new(),
            _body_writer: BodyWriter::empty(),
            _skip_body: false,
        }
    }

    /// For the answer to a HEAD (RFC 7231 4.3.2): the handler's reply is collected as for a
    /// GET, headers included, but its body is not written.
    #[inline]
    pub fn skip_body(&mut self) {
        self._skip_body = true;
    }

    /// Sets header `h` to `value`.
    #[inline]
    pub fn add_header(&mut self, h: &Sstring, value: &Sstring) -> &mut Self {
        self._headers.insert(CaseInsensitiveKey(h.clone()), value.clone());
        self
    }

    /// Returns the value of the header `name` (compared case-insensitively), or an empty
    /// string if the reply has no such header.
    #[inline]
    pub fn get_header(&self, name: &str) -> Sstring {
        match self._headers.get(CaseInsensitiveStr::new(name)) {
            None => Sstring::new(),
            Some(v) => v.clone(),
        }
    }

    /// Sets the http version.
    #[inline]
    pub fn set_version(&mut self, version: &Sstring) -> &mut Self {
        self._version = version.clone();
        self
    }

    /// Sets the status and, when not empty, the content.
    #[inline]
    pub fn set_status(&mut self, status: StatusType, content: Sstring) -> &mut Self {
        self._status = status;
        if !content.empty() {
            self._content = content;
        }
        self
    }

    /// Sets the `Content-Type` header to the MIME type `mime`, and returns `self`.
    ///
    /// Use it when the MIME type is known; in most cases [`Reply::set_content_type`] is
    /// simpler.
    #[inline]
    pub fn set_mime_type(&mut self, mime: &Sstring) -> &mut Self {
        self._headers.insert(CaseInsensitiveKey(Sstring::from("Content-Type")), mime.clone());
        self
    }

    /// Sets the `Content-Type` header from the file extension the content would have as a
    /// file, such as `html`, `txt` or `json`, and returns `self`.
    #[inline]
    pub fn set_content_type(&mut self, content_type: &str) -> &mut Self {
        let mime = Sstring::from(mime_types::extension_to_type(content_type));
        self.set_mime_type(&mime);
        self
    }

    /// `set_content_type()` with the default "html".
    #[inline]
    pub fn set_content_type_default(&mut self) -> &mut Self {
        self.set_content_type("html")
    }

    /// Sets the content type, then the response line.
    #[inline]
    pub fn done_with(&mut self, content_type: &str) -> &mut Self {
        self.set_content_type(content_type).done()
    }

    /// Finishes the reply by computing its response line from the status and version, and
    /// returns `self`. Call it before the reply is sent.
    #[inline]
    pub fn done(&mut self) -> &mut Self {
        self._response_line = self.response_line();
        self
    }

    /// The response line: `HTTP/<version> <status>\r\n`.
    ///
    /// The line is built in one allocation of its exact size (none when it fits inline),
    /// with the parts copied straight into it.
    pub fn response_line(&self) -> Sstring {
        // A status with no known reason phrase is sent with its own code and an empty
        // phrase: the space after the code stays (RFC 9112 4: `HTTP-version SP status-code SP
        // [ reason-phrase ]`).
        let unknown;
        let status = match status_strings::find(self._status) {
            Some(s) => s.as_bytes(),
            None => {
                unknown = format!("{} ", self._status.0);
                unknown.as_bytes()
            }
        };
        let version = self._version.as_bytes();
        let parts: [&[u8]; 5] = [b"HTTP/", version, b" ", status, b"\r\n"];
        let mut line =
            crate::engine::core::sstring::uninitialized_string(parts.iter().map(|p| p.len()).sum());
        let dst = line.as_bytes_mut();
        let mut off = 0;
        for p in parts {
            dst[off..off + p.len()].copy_from_slice(p);
            off += p.len();
        }
        line
    }

    /// Sets a writer that writes the body to an output stream; the reply is then sent
    /// with chunked transfer encoding.
    ///
    /// `content_type` chooses the content type, as the file extension such content would
    /// have (`"txt"`, `"html"`, `"json"`...). `body_writer` receives the output stream and
    /// writes the body to it; it owns the stream while it uses it and must close it when
    /// done.
    pub fn write_body_writer(&mut self, content_type: &str, body_writer: BodyWriter) {
        self.set_content_type(content_type);
        self._body_writer = body_writer;
    }

    /// Sets a writer that writes the body to an output stream, sent with chunked transfer
    /// encoding.
    ///
    /// As [`Reply::write_body_writer`], except that `body_writer` only writes to the
    /// stream: the stream is flushed and closed automatically once its future resolves.
    #[inline]
    pub fn write_body_with<W>(&mut self, content_type: &str, body_writer: W)
    where
        W: FnOnce(&mut OutputStream<u8>) -> Future<()> + 'static,
    {
        self.write_body_writer(content_type, closing_body_writer(body_writer));
    }

    /// Sets `content` as the body and finishes the reply.
    ///
    /// `content_type` chooses the content type, as for [`Reply::write_body_writer`]. Sets
    /// the content and the content type, then calls [`Reply::done`].
    pub fn write_body(&mut self, content_type: &str, content: Sstring) {
        self._content = content;
        self.done_with(content_type);
    }

    /// Writes the reply to `con` with chunked transfer encoding: the response line, the
    /// headers, then the body through the body writer.
    ///
    /// # Safety
    /// The reply and the connection stay alive and in place until the returned future
    /// resolves, and the reply is not used otherwise meanwhile (the continuations reach both
    /// through their addresses).
    pub(crate) unsafe fn write_reply_to_connection(&mut self, con: &mut Connection) -> Future<()> {
        self.add_header(&Sstring::from("Transfer-Encoding"), &Sstring::from("chunked"));
        let this: *mut Reply = self;
        let conp: *mut Connection = con;
        con.out()
            .write_sstring(&self.response_line())
            // SAFETY: the reply and the connection are alive until the write resolves (the
            // contract above), so `this` and `conp` are valid in every continuation.
            .then(move |()| unsafe { (*this).write_reply_headers(&mut *conp) })
            // SAFETY: as above; the header write has finished with the connection.
            .then(move |()| unsafe { (*conp).out() }.write(b"\r\n"))
            // SAFETY: as above; the previous writes have finished with the connection.
            .then(move |()| unsafe {
                if (*this)._skip_body {
                    return make_ready_future(());
                }
                (*this)._body_writer.call(make_http_chunked_output_stream((*conp).out()))
            })
    }

    /// Writes the header lines of the reply to `con`, one write per header.
    ///
    /// # Safety
    /// As [`Reply::write_reply_to_connection`]; the headers are not changed until the
    /// returned future resolves.
    pub(crate) unsafe fn write_reply_headers(&mut self, con: &mut Connection) -> Future<()> {
        let conp: *mut Connection = con;
        let headers: *const HeaderMap = &self._headers;
        // SAFETY: the contract above: the reply is alive and its headers unchanged until the
        // future resolves.
        do_for_each(unsafe { (*headers).iter() }, move |(k, v): (&CaseInsensitiveKey, &Sstring)| {
            let mut line = Sstring::new();
            line.append_bytes(k.0.as_bytes());
            line.append_bytes(b": ");
            line.append_bytes(v.as_bytes());
            line.append_bytes(b"\r\n");
            // SAFETY: as in write_reply_to_connection().
            unsafe { (*conp).out() }.write_sstring(&line)
        })
    }
}

/// Wraps a writer that only writes into the stream into a [`BodyWriter`] that also
/// flushes and closes it (used by the `write_body` methods).
#[inline]
pub(crate) fn closing_body_writer<W>(body_writer: W) -> BodyWriter
where
    W: FnOnce(&mut OutputStream<u8>) -> Future<()> + 'static,
{
    let mut body_writer = Some(body_writer);
    BodyWriter::new(move |out: OutputStream<u8>| -> Future<()> {
        // The body writer is called once, which moves the writer into
        // write_to_stream_and_close(); a further call only closes the stream.
        match body_writer.take() {
            Some(w) => write_to_stream_and_close(out, w),
            None => {
                let mut out = out;
                out.close()
            }
        }
    })
}

mod status_strings {
    use super::StatusType;

    #[inline]
    pub(super) fn find(status: StatusType) -> Option<&'static str> {
        Some(match status {
            StatusType::CONTINUE => "100 Continue",
            StatusType::SWITCHING_PROTOCOLS => "101 Switching Protocols",
            StatusType::OK => "200 OK",
            StatusType::CREATED => "201 Created",
            StatusType::ACCEPTED => "202 Accepted",
            StatusType::NONAUTHORITATIVE_INFORMATION => "203 Non-Authoritative Information",
            StatusType::NO_CONTENT => "204 No Content",
            StatusType::RESET_CONTENT => "205 Reset Content",
            StatusType::PARTIAL_CONTENT => "206 Partial Content",
            StatusType::MULTIPLE_CHOICES => "300 Multiple Choices",
            StatusType::MOVED_PERMANENTLY => "301 Moved Permanently",
            StatusType::MOVED_TEMPORARILY => "302 Moved Temporarily",
            StatusType::SEE_OTHER => "303 See Other",
            StatusType::NOT_MODIFIED => "304 Not Modified",
            StatusType::USE_PROXY => "305 Use Proxy",
            StatusType::TEMPORARY_REDIRECT => "307 Temporary Redirect",
            StatusType::PERMANENT_REDIRECT => "308 Permanent Redirect",
            StatusType::BAD_REQUEST => "400 Bad Request",
            StatusType::UNAUTHORIZED => "401 Unauthorized",
            StatusType::PAYMENT_REQUIRED => "402 Payment Required",
            StatusType::FORBIDDEN => "403 Forbidden",
            StatusType::NOT_FOUND => "404 Not Found",
            StatusType::METHOD_NOT_ALLOWED => "405 Method Not Allowed",
            StatusType::NOT_ACCEPTABLE => "406 Not Acceptable",
            StatusType::REQUEST_TIMEOUT => "408 Request Timeout",
            StatusType::CONFLICT => "409 Conflict",
            StatusType::GONE => "410 Gone",
            StatusType::LENGTH_REQUIRED => "411 Length Required",
            StatusType::PAYLOAD_TOO_LARGE => "413 Payload Too Large",
            StatusType::URI_TOO_LONG => "414 URI Too Long",
            StatusType::UNSUPPORTED_MEDIA_TYPE => "415 Unsupported Media Type",
            StatusType::EXPECTATION_FAILED => "417 Expectation Failed",
            StatusType::PAGE_EXPIRED => "419 Page Expired",
            StatusType::UNPROCESSABLE_ENTITY => "422 Unprocessable Entity",
            StatusType::UPGRADE_REQUIRED => "426 Upgrade Required",
            StatusType::TOO_MANY_REQUESTS => "429 Too Many Requests",
            StatusType::REQUEST_HEADER_FIELDS_TOO_LARGE => "431 Request Header Fields Too Large",
            StatusType::LOGIN_TIMEOUT => "440 Login Timeout",
            StatusType::INTERNAL_SERVER_ERROR => "500 Internal Server Error",
            StatusType::NOT_IMPLEMENTED => "501 Not Implemented",
            StatusType::BAD_GATEWAY => "502 Bad Gateway",
            StatusType::SERVICE_UNAVAILABLE => "503 Service Unavailable",
            StatusType::GATEWAY_TIMEOUT => "504 Gateway Timeout",
            StatusType::HTTP_VERSION_NOT_SUPPORTED => "505 HTTP Version Not Supported",
            StatusType::INSUFFICIENT_STORAGE => "507 Insufficient Storage",
            StatusType::BANDWIDTH_LIMIT_EXCEEDED => "509 Bandwidth Limit Exceeded",
            StatusType::NETWORK_READ_TIMEOUT => "598 Network Read Timeout",
            StatusType::NETWORK_CONNECT_TIMEOUT => "599 Network Connect Timeout",
            _ => return None,
        })
    }

}
/// Writes the status code and its reason phrase (for example `200 OK`); an unknown code is
/// written as the number alone.
impl fmt::Display for StatusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match status_strings::find(*self) {
            Some(s) => f.write_str(s),
            None => write!(f, "{}", self.0),
        }
    }
}

/// The deprecated alias of [`Reply`] under its older path.
pub mod httpd {
    /// Use http::reply instead.
    #[deprecated(note = "Use http::reply instead")]
    pub type Reply = super::Reply;
}
