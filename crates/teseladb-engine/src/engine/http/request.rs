//! HTTP requests: the [`Request`] a server handler receives and a client sends.
//!
//! A [`Request`] holds the method, URL, version, headers, query and path parameters and,
//! for client requests, a body writer. The server's connection loop fills one per parsed
//! request on the shard that owns the connection; handlers on that shard read it without
//! synchronisation.
//!
//! Header names are compared case-insensitively ([`CaseInsensitiveKey`]); query parameters
//! are URL-decoded when [`Request::parse_query_param`] splits the URL.

use std::cell::Cell;
use std::collections::HashMap;

use crate::engine::core::future::Future;
use crate::engine::core::iostream::{InputStream, OutputStream};
use crate::engine::core::loop_::do_for_each;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::{OperationType, Parameters, type2str};
use crate::engine::http::mime_types::mime_types;
use crate::engine::http::reply::{BodyWriter, HeaderMap, closing_body_writer};
use crate::engine::http::url::internal::{url_decode, url_encode};
use crate::engine::net::socket_defs::SocketAddress;
use crate::engine::util::string_utils::internal::{
    CaseInsensitiveCmp, CaseInsensitiveKey, CaseInsensitiveStr,
};
use crate::engine_assert;

/// The class of the request content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Ctclass {
    /// Anything else.
    #[default]
    Other,
    /// multipart/*
    Multipart,
    /// application/x-www-form-urlencoded
    AppXWwwUrlencoded,
}

/// An HTTP request: one the server received from a client, or one a client builds to
/// send.
pub struct Request {
    /// The address of the client.
    pub _client_address: SocketAddress,
    /// The address of the server.
    pub _server_address: SocketAddress,
    /// The method.
    pub _method: Sstring,
    /// The url.
    pub _url: Sstring,
    /// The http version.
    pub _version: Sstring,
    /// The content type class.
    pub content_type_class: Ctclass,
    /// The content length.
    pub content_length: usize,
    /// Bytes of the body written so far; a `Cell` so the body writer can update it through `&self`.
    pub _bytes_written: Cell<usize>,
    /// The headers.
    pub _headers: HeaderMap,
    /// The query parameters.
    pub query_parameters: HashMap<Sstring, Sstring>,
    /// The path parameters.
    pub param: Parameters,
    /// server-side deprecated: use content_stream instead
    pub content: Sstring,
    /*
     * The handler should read the contents of this stream till reaching eof (i.e., the end of this request's content). Failing to do so
     * will force the server to close this connection, and the client will not be able to reuse this connection for the next request.
     * The stream should not be closed by the handler, the server will close it for the handler.
     * */
    /// The content of the request, owned by the server.
    pub content_stream: *mut InputStream<u8>,
    /// The trailing headers of a chunked body.
    pub trailing_headers: HashMap<Sstring, Sstring>,
    /// The chunk extensions of a chunked body.
    pub chunk_extensions: HashMap<Sstring, Sstring>,
    /// "http" or "https".
    pub protocol_name: Sstring,
    /// for client
    pub body_writer: BodyWriter,
}

impl Default for Request {
    fn default() -> Self {
        Request {
            _client_address: SocketAddress::default(),
            _server_address: SocketAddress::default(),
            _method: Sstring::new(),
            _url: Sstring::new(),
            _version: Sstring::new(),
            content_type_class: Ctclass::Other,
            content_length: 0,
            _bytes_written: Cell::new(0),
            _headers: HeaderMap::default(),
            query_parameters: HashMap::new(),
            param: Parameters::default(),
            content: Sstring::new(),
            content_stream: std::ptr::null_mut(),
            trailing_headers: HashMap::new(),
            chunk_extensions: HashMap::new(),
            protocol_name: Sstring::from("http"),
            body_writer: BodyWriter::empty(),
        }
    }
}

impl Request {
    /// Returns the address of the client that sent the request.
    #[inline]
    pub fn get_client_address(&self) -> &SocketAddress {
        &self._client_address
    }

    /// Returns the address of the server that received the request.
    #[inline]
    pub fn get_server_address(&self) -> &SocketAddress {
        &self._server_address
    }

    /// Returns the value of the header `name` (compared case-insensitively), or an empty
    /// string if the request has no such header.
    #[inline]
    pub fn get_header(&self, name: &str) -> Sstring {
        match self._headers.get(CaseInsensitiveStr::new(name)) {
            None => Sstring::new(),
            Some(v) => v.clone(),
        }
    }

    /// Returns the value of the query parameter `key`, or an empty string if there is no
    /// such parameter. When the query repeats the key, the last value wins.
    #[inline]
    pub fn get_query_param(&self, key: &str) -> Sstring {
        match self.query_parameters.get(key.as_bytes()) {
            None => Sstring::new(),
            Some(v) => v.clone(),
        }
    }

    /// Returns the URL-decoded value of the path parameter `key`, or an empty string if the
    /// parameter does not exist or cannot be decoded.
    #[inline]
    pub fn get_path_param(&self, key: &Sstring) -> Sstring {
        self.param.get_decoded_param(key)
    }

    /// Returns the request's protocol name: `"http"` or `"https"`.
    #[inline]
    pub fn get_protocol_name(&self) -> Sstring {
        self.protocol_name.clone()
    }

    /// Returns the full request URL: `<protocol>://<Host header><url>`.
    pub fn get_url(&self) -> Sstring {
        let host = self._headers.get(CaseInsensitiveStr::new("Host"));
        let host = host.map_or(&[][..], |h| h.as_bytes());
        let mut r = Sstring::new();
        r.append_bytes(self.protocol_name.as_bytes());
        r.append_bytes(b"://");
        r.append_bytes(host);
        r.append_bytes(self._url.as_bytes());
        r
    }

    /// Whether the content is multipart.
    #[inline]
    pub fn is_multi_part(&self) -> bool {
        self.content_type_class == Ctclass::Multipart
    }

    /// Whether the content is an url-encoded form.
    #[inline]
    pub fn is_form_post(&self) -> bool {
        self.content_type_class == Ctclass::AppXWwwUrlencoded
    }

    /// Whether the connection stays open after this request.
    pub fn should_keep_alive(&self) -> bool {
        if self._version.as_bytes() == b"0.9" {
            return false;
        }

        // TODO: handle HTTP/2.0 when it releases

        let it = self._headers.get(CaseInsensitiveStr::new("Connection"));
        if self._version.as_bytes() == b"1.0" {
            it.is_some_and(|v| CaseInsensitiveCmp.call(v.as_bytes(), b"keep-alive"))
        } else {
            // HTTP/1.1
            it.is_none_or(|v| !CaseInsensitiveCmp.call(v.as_bytes(), b"close"))
        }
    }

    /// Parses the query parameters of the request URL into the request, and returns the
    /// URL's path part, without the query.
    ///
    /// The query follows the `?` and its parameters are separated by `&`; keys and values
    /// are URL-decoded, and a parameter that fails to decode is skipped.
    pub fn parse_query_param(&mut self) -> Sstring {
        // The URL is read in place, without a copy, while the parameters (another field)
        // are filled.
        let bytes = self._url.as_bytes();
        let pos = match bytes.iter().position(|&c| c == b'?') {
            None => return self._url.clone(),
            Some(p) => p,
        };
        let params = &mut self.query_parameters;
        let mut curr = pos + 1;
        while let Some(off) = bytes[curr..].iter().position(|&c| c == b'&') {
            let end_param = curr + off;
            Self::add_query_param(params, &bytes[curr..end_param]);
            curr = end_param + 1;
        }
        Self::add_query_param(params, &bytes[curr..]);
        Sstring::from_bytes(&bytes[..pos])
    }

    /// Builds the URL string from the request URL and its query parameters (URL-encoded),
    /// in the form [`Request::parse_query_param`] parses.
    pub fn format_url(&self) -> Sstring {
        let mut query = Sstring::new();
        let mut delim: &[u8] = b"?";
        for (k, v) in &self.query_parameters {
            query.append_bytes(delim);
            query.append_bytes(url_encode(k.as_bytes()).as_bytes());
            if !v.empty() {
                query.append_bytes(b"=");
                query.append_bytes(url_encode(v.as_bytes()).as_bytes());
            }
            delim = b"&";
        }
        let mut r = Sstring::new();
        r.append_bytes(self._url.as_bytes());
        r.append_bytes(query.as_bytes());
        r
    }

    /// Sets the `Content-Type` header to the MIME type `mime`.
    ///
    /// Use it when the MIME type is known; in most cases [`Request::set_content_type`] is
    /// simpler.
    #[inline]
    pub fn set_mime_type(&mut self, mime: &Sstring) {
        self._headers.insert(CaseInsensitiveKey(Sstring::from("Content-Type")), mime.clone());
    }

    /// Sets the `Content-Type` header from the file extension the content would have as a
    /// file, such as `html`, `txt` or `json`.
    #[inline]
    pub fn set_content_type(&mut self, content_type: &str) {
        self.set_mime_type(&Sstring::from(mime_types::extension_to_type(content_type)));
    }

    /// Sets `content` as the body.
    ///
    /// `content_type` chooses the content type, as the file extension such content would
    /// have (`"txt"`, `"html"`, `"json"`...). Sets the content, the content length and
    /// the content type. Suited to a body available as one contiguous buffer.
    pub fn write_body(&mut self, content_type: &str, content: Sstring) {
        self.set_content_type(content_type);
        self.content_length = content.size();
        self.content = content;
    }

    /// Sets a writer that writes the body to an output stream, sent with chunked transfer
    /// encoding.
    ///
    /// `content_type` chooses the content type, as for [`Request::write_body`].
    /// `body_writer` receives the output stream and writes the body to it; it owns the
    /// stream while it uses it and must close it when done.
    ///
    /// Suited to a body of unknown or hard to compute length, such as the contents of
    /// another input stream or a body held in several buffers.
    pub fn write_body_writer(&mut self, content_type: &str, body_writer: BodyWriter) {
        self.set_content_type(content_type);
        self._headers.insert(
            CaseInsensitiveKey(Sstring::from("Transfer-Encoding")),
            Sstring::from("chunked"),
        );
        self.body_writer = body_writer;
    }

    /// Sets a writer that writes the body to an output stream, sent with chunked transfer
    /// encoding.
    ///
    /// As [`Request::write_body_writer`], except that `body_writer` only writes to the
    /// stream: the stream is flushed and closed automatically once its future resolves.
    #[inline]
    pub fn write_body_with<W>(&mut self, content_type: &str, body_writer: W)
    where
        W: FnOnce(&mut OutputStream<u8>) -> Future<()> + 'static,
    {
        self.write_body_writer(content_type, closing_body_writer(body_writer));
    }

    /// Sets a writer that writes a body of `len` bytes to an output stream, sent with a
    /// `Content-Length` header.
    ///
    /// `content_type` chooses the content type, as for [`Request::write_body`].
    /// `body_writer` receives the output stream and writes the body to it; it owns the
    /// stream while it uses it and must close it when done. With `len` 0 the writer is
    /// not kept.
    ///
    /// Suited to a body that is not one contiguous buffer but whose size is known, or to a
    /// server that requires a `Content-Length` header. If the writer writes fewer or more
    /// than `len` bytes, sending the request fails.
    pub fn write_body_len_writer(
        &mut self,
        content_type: &str,
        len: usize,
        body_writer: BodyWriter,
    ) {
        self.set_content_type(content_type);
        self.content_length = len;
        if len > 0 {
            // At the time of this writing, connection::write_body()
            // assumes that `body_writer` is unset if `content_length` is 0.
            self.body_writer = body_writer;
        }
    }

    /// Sets a writer that writes a body of `len` bytes to an output stream, sent with a
    /// `Content-Length` header.
    ///
    /// As [`Request::write_body_len_writer`], except that `body_writer` only writes to the
    /// stream: the stream is flushed and closed automatically once its future resolves.
    #[inline]
    pub fn write_body_len_with<W>(&mut self, content_type: &str, len: usize, body_writer: W)
    where
        W: FnOnce(&mut OutputStream<u8>) -> Future<()> + 'static,
    {
        self.write_body_len_writer(content_type, len, closing_body_writer(body_writer));
    }

    /// Makes the request send an `Expect: 100-continue` header.
    ///
    /// The client connection then sends the headers and waits for the server's reply
    /// before transferring the body (see
    /// [`Connection::make_request`](crate::engine::http::client::experimental::Connection::make_request)).
    pub fn set_expects_continue(&mut self) {
        self._headers
            .insert(CaseInsensitiveKey(Sstring::from("Expect")), Sstring::from("100-continue"));
    }

    /// Creates a request with the method `method` (such as `"GET"` or `"POST"`) for the URL
    /// `path`, with `host` as its `Host` header.
    pub fn make(method: Sstring, host: Sstring, path: Sstring) -> Request {
        let mut rq = Request { _method: method, _url: path, ..Request::default() };
        rq._headers.insert(CaseInsensitiveKey(Sstring::from("Host")), host);
        rq
    }

    /// Creates a request with the method of `type_` (such as [`OperationType::Get`]) for
    /// the URL `path`, with `host` as its `Host` header.
    pub fn make_op(type_: OperationType, host: Sstring, path: Sstring) -> Request {
        Self::make(Sstring::from(type2str(type_)), host, path)
    }

    /// The request line: `<method> <url> HTTP/<version>\r\n`.
    pub fn request_line(&self) -> Sstring {
        engine_assert!(!self._version.empty());
        let url = self.format_url();
        let mut r = Sstring::new();
        r.append_bytes(self._method.as_bytes());
        r.append_bytes(b" ");
        r.append_bytes(url.as_bytes());
        r.append_bytes(b" HTTP/");
        r.append_bytes(self._version.as_bytes());
        r.append_bytes(b"\r\n");
        r
    }

    // FIXME -- generalize with reply::write_request_headers
    /// Writes the header lines into `out`.
    ///
    /// # Safety
    /// The request and `out` stay alive and in place until the returned future resolves;
    /// meanwhile the headers are not changed and `out` is not used otherwise: the returned
    /// future iterates the header map and writes to `out` through raw pointers.
    pub unsafe fn write_request_headers(&self, out: &mut OutputStream<u8>) -> Future<()> {
        let outp: *mut OutputStream<u8> = out;
        let headers: *const HeaderMap = &self._headers;
        // SAFETY: the contract above.
        do_for_each(unsafe { (*headers).iter() }, move |(k, v): (&CaseInsensitiveKey, &Sstring)| {
            let mut line = Sstring::new();
            line.append_bytes(k.0.as_bytes());
            line.append_bytes(b": ");
            line.append_bytes(v.as_bytes());
            line.append_bytes(b"\r\n");
            // SAFETY: the contract above.
            unsafe { (*outp).write_sstring(&line) }
        })
    }

    fn add_query_param(query_parameters: &mut HashMap<Sstring, Sstring>, param: &[u8]) {
        let split = param.iter().position(|&c| c == b'=').unwrap_or(usize::MAX);

        // `param.len() - 1` wraps to `usize::MAX` for an empty param, so an empty
        // param takes the "key only" branch.
        if split >= param.len().wrapping_sub(1) {
            let mut key = Sstring::new();
            if url_decode(&param[..split.min(param.len())], &mut key) {
                query_parameters.insert(key, Sstring::new());
            }
        } else {
            let mut key = Sstring::new();
            let mut value = Sstring::new();
            if url_decode(&param[..split], &mut key) && url_decode(&param[split + 1..], &mut value)
            {
                query_parameters.insert(key, value);
            }
        }
    }
}

/// The deprecated alias of [`Request`] under its older path.
pub mod httpd {
    /// Use http::request instead.
    #[deprecated(note = "Use http::request instead")]
    pub type Request = super::Request;
}
