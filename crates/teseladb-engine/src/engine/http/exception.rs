//! HTTP errors that handlers return to produce an error reply.
//!
//! Every error is one type, [`BaseException`], carrying a message, a status code and an
//! [`ExceptionKind`] that tells the variety (bad request, not found, redirect, ...).
//! Constructors such as [`not_found_exception`] or [`redirect_exception`] build each
//! variety; predicates such as `e.is_bad_request()` test it, and also match the varieties
//! that refine it. [`JsonException`] renders an error as the JSON body of the reply.

use std::fmt;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::reply::StatusType;
use crate::engine::json::json_elements::{
    JsonBase, JsonBaseElement, JsonElement, Jsonable, json_base_add,
};
use crate::engine::util::log::DisplayExceptionPtr;

/// The variety of an HTTP error; the refinements of bad request and server error also
/// count as their parent variety for the `is_*` predicates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExceptionKind {
    /// A plain error with its own message and status.
    Base,
    /// A redirect ([`redirect_exception`]), with the target url.
    Redirect(String),
    /// 404 not found ([`not_found_exception`]).
    NotFound,
    /// 400 bad request ([`bad_request_exception`]).
    BadRequest,
    /// A bad parameter ([`bad_param_exception`]), a kind of bad request.
    BadParam,
    /// A missing mandatory parameter ([`missing_param_exception`]), a kind of bad request.
    MissingParam,
    /// A malformed body chunk ([`bad_chunk_exception`]), a kind of bad request.
    BadChunk,
    /// 500 internal server error ([`server_error_exception`]).
    ServerError,
    /// A response the client could not parse ([`response_parsing_exception`]), a kind of
    /// server error.
    ResponseParsing,
    /// A response with a status the caller did not expect ([`unexpected_status_error`]).
    UnexpectedStatus,
}

/// The base of all HTTP errors: a message, returned as the reply content, and a status,
/// returned as the status code.
#[derive(Clone, Debug)]
pub struct BaseException {
    _msg: String,
    _status: StatusType,
    _kind: ExceptionKind,
}

impl BaseException {
    /// A base exception with a message and a status.
    pub fn new(msg: impl Into<String>, status: StatusType) -> Self {
        BaseException { _msg: msg.into(), _status: status, _kind: ExceptionKind::Base }
    }

    fn with_kind(msg: String, status: StatusType, kind: ExceptionKind) -> Self {
        BaseException { _msg: msg, _status: status, _kind: kind }
    }

    /// The message.
    pub fn what(&self) -> &str {
        &self._msg
    }

    /// The http status.
    pub fn status(&self) -> StatusType {
        self._status
    }

    /// The message.
    pub fn str(&self) -> &String {
        &self._msg
    }

    /// The derived class.
    pub fn kind(&self) -> &ExceptionKind {
        &self._kind
    }

    /// Whether this is a bad_request_exception (or derived from it).
    pub fn is_bad_request(&self) -> bool {
        matches!(
            self._kind,
            ExceptionKind::BadRequest
                | ExceptionKind::BadParam
                | ExceptionKind::MissingParam
                | ExceptionKind::BadChunk
        )
    }

    /// Whether this is a server_error_exception (or derived from it).
    pub fn is_server_error(&self) -> bool {
        matches!(self._kind, ExceptionKind::ServerError | ExceptionKind::ResponseParsing)
    }

    /// The url of a redirect_exception.
    pub fn url(&self) -> Option<&str> {
        match &self._kind {
            ExceptionKind::Redirect(u) => Some(u),
            _ => None,
        }
    }
}

impl std::error::Error for BaseException {}

/// Writes the message and the status of the error: `message (status)`.
impl fmt::Display for BaseException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.what(), self.status())
    }
}

/// Returns an error that, returned by a handler, redirects the client to `url` with
/// the status `status`.
pub fn redirect_exception(url: impl Into<String>, status: StatusType) -> BaseException {
    BaseException::with_kind(String::new(), status, ExceptionKind::Redirect(url.into()))
}

/// [`redirect_exception`] with the default status (moved permanently).
pub fn redirect_exception_default(url: impl Into<String>) -> BaseException {
    redirect_exception(url, StatusType::MOVED_PERMANENTLY)
}

/// Returns an error that, returned by a handler, produces a `404 Not Found` reply with
/// the message `msg`.
pub fn not_found_exception(msg: impl Into<String>) -> BaseException {
    BaseException::with_kind(msg.into(), StatusType::NOT_FOUND, ExceptionKind::NotFound)
}

/// [`not_found_exception`] with the default message, "Not found".
pub fn not_found_exception_default() -> BaseException {
    not_found_exception("Not found")
}

/// Returns an error that, returned by a handler, produces a `400 Bad Request` reply with
/// the message `msg`.
pub fn bad_request_exception(msg: impl Into<String>) -> BaseException {
    BaseException::with_kind(msg.into(), StatusType::BAD_REQUEST, ExceptionKind::BadRequest)
}

/// A 400 bad request error for a parameter with a bad value.
pub fn bad_param_exception(msg: impl Into<String>) -> BaseException {
    BaseException::with_kind(msg.into(), StatusType::BAD_REQUEST, ExceptionKind::BadParam)
}

/// A 400 bad request error naming a missing mandatory parameter.
pub fn missing_param_exception(param: &str) -> BaseException {
    BaseException::with_kind(
        format!("Missing mandatory parameter '{}'", param),
        StatusType::BAD_REQUEST,
        ExceptionKind::MissingParam,
    )
}

/// A 400 bad request error for a body chunk that cannot be read.
pub fn bad_chunk_exception(msg: &str) -> BaseException {
    BaseException::with_kind(
        format!("Can't read body chunk in a 'chunked' request '{}'", msg),
        StatusType::BAD_REQUEST,
        ExceptionKind::BadChunk,
    )
}

/// A 500 internal server error with the given message.
pub fn server_error_exception(msg: impl Into<String>) -> BaseException {
    BaseException::with_kind(
        msg.into(),
        StatusType::INTERNAL_SERVER_ERROR,
        ExceptionKind::ServerError,
    )
}

/// A 500 error for a response that could not be parsed.
pub fn response_parsing_exception(msg: impl Into<String>) -> BaseException {
    BaseException::with_kind(
        msg.into(),
        StatusType::INTERNAL_SERVER_ERROR,
        ExceptionKind::ResponseParsing,
    )
}

/// An error carrying a response status the caller did not expect; the message is the
/// status line text.
pub fn unexpected_status_error(st: StatusType) -> BaseException {
    BaseException::with_kind(st.to_string(), st, ExceptionKind::UnexpectedStatus)
}

/// The json body of an error reply: `{"message": ..., "code": ...}`.
///
/// The members are named at construction and registered with a [`JsonBase`] on demand,
/// so the object holds no self-references and can move freely.
pub struct JsonException {
    /// The message.
    pub _msg: JsonElement<Sstring>,
    /// The status code.
    pub _code: JsonElement<i32>,
}

impl JsonException {
    fn register_params(&mut self) {
        json_base_add(&mut self._msg, "message", false);
        json_base_add(&mut self._code, "code", false);
    }

    fn empty() -> Self {
        JsonException { _msg: JsonElement::default(), _code: JsonElement::default() }
    }

    /// From an http exception.
    pub fn from_base(e: &BaseException) -> Self {
        let mut j = Self::empty();
        j.set(Sstring::from(e.str().as_str()), e.status());
        j
    }

    /// From any exception.
    pub fn from_exception_ptr(e: &ExceptionPtr) -> Self {
        let exception_description = DisplayExceptionPtr(Some(e)).to_string();
        let mut j = Self::empty();
        j.set(Sstring::from(exception_description), StatusType::INTERNAL_SERVER_ERROR);
        j
    }

    fn set(&mut self, msg: Sstring, code: StatusType) {
        self.register_params();
        self._msg.set(msg);
        self._code.set(code.0);
    }
}

impl JsonBase for JsonException {
    fn element_count(&self) -> usize {
        2
    }

    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        match i {
            0 => Some(&self._msg),
            1 => Some(&self._code),
            _ => None,
        }
    }
}

impl Jsonable for JsonException {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        self.base_to_json()
    }
    fn write(
        self: Box<Self>,
        s: &mut crate::engine::core::iostream::OutputStreamState<u8>,
    ) -> crate::engine::core::future::Future<()> {
        (*self).base_write(s)
    }
}
