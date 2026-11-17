//! Handlers built from plain functions and closures.
//!
//! [`FunctionHandler`] adapts a closure to [`HandlerBase`]:
//! a closure returning a string body, one that fills the reply itself, one returning a JSON
//! value (synchronously or as a future), or one that takes ownership of the request and
//! reply. The function types are boxed `Fn` objects; a function that fails returns `Err`,
//! which the server turns into an error reply.

use std::rc::Rc;

use crate::engine::core::future::{ExceptionPtr, Future, make_exception_future, make_ready_future};
use crate::engine::core::sstring::Sstring;
use crate::engine::http::handlers::{ConstReq, HandlerBase, HandlerBaseData};
use crate::engine::http::reply::Reply;
use crate::engine::http::request::Request;
use crate::engine::json::json_elements::JsonReturnType;

/// A handler function that receives only the request and returns the reply content.
pub type RequestFunction = Box<dyn Fn(ConstReq<'_>) -> Result<Sstring, ExceptionPtr>>;

/// A handler function that receives the request and the reply, and returns the reply
/// content.
pub type HandleFunction = Box<dyn Fn(ConstReq<'_>, &mut Reply) -> Result<Sstring, ExceptionPtr>>;

/// A handler function that receives only the request and returns a JSON response, which
/// the handler writes as the reply.
pub type JsonRequestFunction = Box<dyn Fn(ConstReq<'_>) -> Result<JsonReturnType, ExceptionPtr>>;

/// A handler function that returns a future JSON response; as with
/// [`JsonRequestFunction`], the handler writes the response as the reply.
pub type FutureJsonFunction = Box<dyn Fn(Box<Request>) -> Future<JsonReturnType>>;

/// A function that fills the reply asynchronously.
pub type FutureHandlerFunction = Rc<dyn Fn(Box<Request>, Box<Reply>) -> Future<Box<Reply>>>;

/// A handler that calls a function, given at construction, to produce the reply.
///
/// Suited to very simple handlers.
#[derive(Clone)]
pub struct FunctionHandler {
    base: HandlerBaseData,
    /// The function that fills the reply.
    pub _f_handle: FutureHandlerFunction,
    /// The content type of the reply.
    pub _type: Sstring,
}

impl FunctionHandler {
    /// From a function of the request and the reply that returns content to append.
    pub fn from_handle_function(f_handle: HandleFunction, type_: &Sstring) -> Self {
        Self::from_future_handler_function(
            Rc::new(move |req: Box<Request>, mut rep: Box<Reply>| match f_handle(&req, &mut rep) {
                Ok(s) => {
                    rep._content.append_bytes(s.as_bytes());
                    make_ready_future(rep)
                }
                Err(e) => make_exception_future(e),
            }),
            type_,
        )
    }

    /// From a function that fills the reply asynchronously.
    pub fn from_future_handler_function(f_handle: FutureHandlerFunction, type_: &Sstring) -> Self {
        FunctionHandler {
            base: HandlerBaseData::default(),
            _f_handle: f_handle,
            _type: type_.clone(),
        }
    }

    /// From a function of the request that returns content to append.
    pub fn from_request_function(handle: RequestFunction, type_: &Sstring) -> Self {
        Self::from_future_handler_function(
            Rc::new(move |req: Box<Request>, mut rep: Box<Reply>| {
                // The content is appended as it is: a plain text reply is not formatted as
                // json (which would quote it).
                match handle(&req) {
                    Ok(s) => {
                        rep._content.append_bytes(s.as_bytes());
                        make_ready_future(rep)
                    }
                    Err(e) => make_exception_future(e),
                }
            }),
            type_,
        )
    }

    /// From a function of the request that returns a json result.
    pub fn from_json_request_function(handle: JsonRequestFunction) -> Self {
        Self::from_future_handler_function(
            Rc::new(move |req: Box<Request>, rep: Box<Reply>| match handle(&req) {
                Ok(res) => Self::append_result(rep, res),
                Err(e) => make_exception_future(e),
            }),
            &Sstring::from("json"),
        )
    }

    /// From a function of the request that returns a future json result.
    pub fn from_future_json_function(handle: FutureJsonFunction) -> Self {
        Self::from_future_handler_function(
            Rc::new(move |req: Box<Request>, rep: Box<Reply>| {
                handle(req).then(move |res: JsonReturnType| Self::append_result(rep, res))
            }),
            &Sstring::from("json"),
        )
    }

    // send the json payload of result to reply, return the reply pointer
    fn append_result(mut reply: Box<Reply>, result: JsonReturnType) -> Future<Box<Reply>> {
        if let Some(f) = result._body_writer {
            // The JSON body writer becomes the reply's body writer.
            reply.write_body_writer("json", crate::engine::http::reply::BodyWriter::new(f));
        } else {
            reply._content.append_bytes(result._res.as_bytes());
        }
        make_ready_future(reply)
    }
}

impl HandlerBase for FunctionHandler {
    fn base(&self) -> &HandlerBaseData {
        &self.base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        // The continuation takes its own copy of the content type (a short name, kept
        // inline in the string), so the handler may go before the reply is done.
        let type_ = self._type.clone();
        (self._f_handle)(req, rep).then(move |mut rep: Box<Reply>| {
            rep.done_with(type_.as_str());
            make_ready_future(rep)
        })
    }
}
