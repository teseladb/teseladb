//! The request handler interface of the HTTP server.
//!
//! A handler implements [`HandlerBase`]: the routing table calls
//! [`HandlerBase::handle`] with the matched path, the request and a reply to fill, on the
//! shard that accepted the connection. Every handler embeds a [`HandlerBaseData`] (reached
//! through [`HandlerBase::base`]) that carries its list of mandatory query parameters,
//! checked by [`HandlerBase::verify_mandatory_params`] before the handler runs.

use std::any::Any;

use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::sstring::Sstring;
use crate::engine::http::exception::missing_param_exception;
use crate::engine::http::reply::Reply;
use crate::engine::http::request::Request;

/// A shared reference to a request, as handlers that only read it take it.
pub type ConstReq<'a> = &'a Request;

/// The state every handler carries: the names of its mandatory query parameters.
#[derive(Clone, Default, Debug)]
pub struct HandlerBaseData {
    _mandatory_param: Vec<Sstring>,
}

impl HandlerBaseData {
    /// Adds the mandatory parameter `param`, and returns `self`.
    #[inline]
    pub fn mandatory(&mut self, param: &Sstring) -> &mut Self {
        self._mandatory_param.push(param.clone());
        self
    }
}

/// The logic that serves an incoming request.
///
/// Every handler implements [`HandlerBase::handle`] and exposes its [`HandlerBaseData`].
/// The `Any` supertrait lets [`downcast_handler`] recover the concrete handler type.
pub trait HandlerBase: Any {
    /// The shared handler state (mandatory parameters).
    fn base(&self) -> &HandlerBaseData;

    /// The shared handler state (mandatory parameters), mutably.
    fn base_mut(&mut self) -> &mut HandlerBaseData;

    /// Handles the request `req` for the URL path `path`, and resolves to the reply `rep`
    /// filled accordingly.
    fn handle(&mut self, path: &Sstring, req: Box<Request>, rep: Box<Reply>) -> Future<Box<Reply>>;

    /// Adds the mandatory parameter `param`, and returns the handler's shared state.
    #[inline]
    fn mandatory(&mut self, param: &Sstring) -> &mut HandlerBaseData {
        self.base_mut().mandatory(param)
    }

    /// Checks that every mandatory parameter is present, and not empty, in the request
    /// `req`.
    ///
    /// # Errors
    ///
    /// Returns a missing-parameter error (see [`missing_param_exception`]) for the first
    /// parameter that is missing or empty.
    #[inline]
    fn verify_mandatory_params(&self, req: &Request) -> Result<(), ExceptionPtr> {
        for param in &self.base()._mandatory_param {
            if req.get_query_param(param.as_str()).empty() {
                return Err(ExceptionPtr::new(missing_param_exception(param.as_str())));
            }
        }
        Ok(())
    }
}

/// `h` as a pointer to the concrete handler type `T` when it is one, `None` otherwise.
///
/// # Safety
/// `h` points to a live handler, with no `&mut` to it live during the call.
#[inline]
pub unsafe fn downcast_handler<T: HandlerBase>(h: *mut dyn HandlerBase) -> Option<*mut T> {
    // SAFETY: the caller's contract; the shared borrow ends with the type check.
    let any: &dyn Any = unsafe { &*h };
    if any.is::<T>() { Some(h as *mut T) } else { None }
}
