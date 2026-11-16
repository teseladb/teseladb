//! The routing table of the HTTP server.
//!
//! [`Routes`] maps a method and URL to a handler: first an exact-match table per method,
//! then the parameterised [`MatchRule`]s in insertion order, then an optional default
//! handler. It also keeps the exception handlers that turn a handler error into a reply.
//! Each shard of a server has its own `Routes`, used without synchronisation.
//!
//! The routes hold their handlers and rules through raw pointers: most are owned (they
//! came from a `Box` and are freed when the routes drop), some are only borrowed by a
//! [`HandlerRegistration`] or [`RuleRegistration`], which unregister them before going
//! away.

use std::collections::{BTreeMap, HashMap};

use crate::engine::net::hash::FxBuildHasher;

use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::{NUM_OPERATION, OperationType, Parameters, str2type};
use crate::engine::http::exception::{
    BaseException, ExceptionKind, JsonException, not_found_exception,
};
use crate::engine::http::function_handlers::FunctionHandler;
use crate::engine::http::handlers::{HandlerBase, downcast_handler};
use crate::engine::http::json_path::{PathDescription, UrlComponentType};
use crate::engine::http::matchrules::MatchRule;
use crate::engine::http::reply::{Reply, StatusType};
use crate::engine::http::request::Request;
use crate::engine::json::json_elements::Jsonable;

/// A URL pattern for defining a route: a fixed path, optionally followed by a parameter
/// that matches the rest of the URL.
#[derive(Debug, Default)]
pub struct Url {
    /// The fixed path.
    pub _path: Sstring,
    /// The parameter that matches until the end of the url, if not empty.
    pub _param: Sstring,
}

impl Url {
    /// Creates a URL pattern for the fixed path `path`, without a parameter.
    #[inline]
    pub fn new(path: &Sstring) -> Self {
        Url { _path: path.clone(), _param: Sstring::new() }
    }

    /// Sets the parameter named `param` that matches the rest of the URL, and returns
    /// `self`.
    #[inline]
    pub fn remainder(&mut self, param: &Sstring) -> &mut Self {
        self._param = param.clone();
        self
    }
}

/// The cookie of a rule added with [`Routes::add_cookie`].
pub type RuleCookie = u64;

/// A function that turns an exception into a reply, or fails with the exception it
/// does not handle.
pub type ExceptionHandlerFun = Box<dyn Fn(ExceptionPtr) -> Result<Box<Reply>, ExceptionPtr>>;

/// The id of a registered exception handler.
pub type ExceptionHandlerId = usize;

/// The registered exception handlers, in registration order. The routes share the map
/// with the requests in progress (copied on write), so a request's error path never
/// refers to the routes themselves.
type ExceptionHandlers = BTreeMap<ExceptionHandlerId, std::rc::Rc<ExceptionHandlerFun>>;

/// Dispatches requests to handlers according to their method and URL.
///
/// Two mechanisms decide: an exact match, chosen when the URL matches a registered one
/// exactly (an optional trailing slash is permitted), and otherwise the matching rules,
/// evaluated in insertion order. A default handler, if set, takes what neither matches.
pub struct Routes {
    /// The exact-match handlers per operation, hashed with FxHash: the URLs come from the
    /// server's own registrations, so an unkeyed hash is enough, and this map is looked up
    /// on every request.
    _map: [HashMap<Sstring, Owned<dyn HandlerBase>, FxBuildHasher>; NUM_OPERATION],
    _rover: RuleCookie,
    _rules: [BTreeMap<RuleCookie, Owned<MatchRule>>; NUM_OPERATION],
    //default Handler -- for any HTTP Method and Path (/*)
    _default_handler: Option<*mut dyn HandlerBase>,
    _exceptions: std::rc::Rc<ExceptionHandlers>,
    _exception_id: ExceptionHandlerId,
}

/// A handler or a rule of the routes, as a raw pointer: most are owned (`put`, `add`,
/// `add_rule`, `add_cookie`) and some only borrowed (those of [`HandlerRegistration`] and
/// [`RuleRegistration`], which unregister them before going away). The flag tells which,
/// so the routes free only what they own.
struct Owned<T: ?Sized> {
    ptr: *mut T,
    owned: bool,
}

impl<T: ?Sized> Owned<T> {
    /// Gives an owned object back as its box; a borrowed one stays with its owner.
    fn into_box(self) -> Option<Box<T>> {
        let me = std::mem::ManuallyDrop::new(self);
        // SAFETY: an owned pointer came from Box::into_raw and is released only here or in drop().
        me.owned.then(|| unsafe { Box::from_raw(me.ptr) })
    }
}

impl<T: ?Sized> Drop for Owned<T> {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: as in into_box().
            unsafe { drop(Box::from_raw(self.ptr)) };
        }
    }
}

/// Turns an exception into a json reply: the first of `exceptions` that handles it
/// makes the reply, otherwise a JSON error reply with the status of the error.
fn exception_reply_with(exceptions: &ExceptionHandlers, mut eptr: ExceptionPtr) -> Box<Reply> {
    let mut rep = Box::new(Reply::new());
    // go over the register exception handler
    // if one of them handle the exception, return.
    for e in exceptions.values() {
        match e(eptr) {
            Ok(r) => return r,
            // this is needed if there are more then one register exception handler
            // so if the exception handler throw a new exception, they would
            // get the new exception and not the first one.
            Err(n) => eptr = n,
        }
    }
    match eptr.downcast_ref::<BaseException>() {
        Some(e) => match e.kind() {
            ExceptionKind::Redirect(url) => {
                rep = Box::new(Reply::new());
                rep.add_header(&Sstring::from("Location"), &Sstring::from(url.as_str()))
                    .set_status(e.status(), Sstring::new());
            }
            _ => {
                rep.set_status(e.status(), json_body(&JsonException::from_base(e)));
            }
        },
        None => {
            rep.set_status(
                StatusType::INTERNAL_SERVER_ERROR,
                json_body(&JsonException::from_exception_ptr(&eptr)),
            );
        }
    }

    rep.done_with("json");
    rep
}

impl Default for Routes {
    fn default() -> Self {
        Self::new()
    }
}

impl Routes {
    /// Empty routes.
    pub fn new() -> Self {
        Routes {
            _map: std::array::from_fn(|_| HashMap::with_hasher(FxBuildHasher::default())),
            _rover: 0,
            _rules: std::array::from_fn(|_| BTreeMap::new()),
            _default_handler: None,
            _exceptions: std::rc::Rc::new(BTreeMap::new()),
            _exception_id: 0,
        }
    }

    /// Adds `handler` as the exact match for `url` (which should start with `/`) under the
    /// operation `type_`, and returns the routes for chaining.
    ///
    /// On success the routes own the handler and drop it when they are dropped.
    ///
    /// # Errors
    /// Fails with a runtime error if a handler for `url` is already registered under
    /// `type_`; the handler is then dropped.
    pub fn put(
        &mut self,
        type_: OperationType,
        url: &str,
        handler: Box<dyn HandlerBase>,
    ) -> Result<&mut Self, ExceptionPtr> {
        // The pointer comes from Box::into_raw; on failure it is ours again.
        let p = Box::into_raw(handler);
        // SAFETY: `p` is a live, uniquely owned handler from Box::into_raw; with
        // `owned = true` the routes take it over on success and drop it with themselves.
        match unsafe { self.put_raw(type_, url, p, true) } {
            Ok(()) => Ok(self),
            Err(e) => {
                // SAFETY: refused, so still ours.
                unsafe { drop(Box::from_raw(p)) };
                Err(e)
            }
        }
    }

    /// [`Routes::put`] with a raw pointer: `owned` tells whether the routes take ownership (it
    /// came from `Box::into_raw`) or only borrow it (a [`HandlerRegistration`]). On failure the
    /// handler stays with the caller.
    ///
    /// # Safety
    /// `handler` is valid until it is dropped from the routes, or until the routes go away if
    /// it is owned; an owned pointer comes from `Box::into_raw` and is used by nobody else.
    pub(crate) unsafe fn put_raw(
        &mut self,
        type_: OperationType,
        url: &str,
        handler: *mut dyn HandlerBase,
        owned: bool,
    ) -> Result<(), ExceptionPtr> {
        let bucket = &mut self._map[type_ as usize];
        if bucket.contains_key(url.as_bytes()) {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                "Handler for {} already exists.",
                url
            ))));
        }
        bucket.insert(Sstring::from(url), Owned { ptr: handler, owned });
        Ok(())
    }

    /// Removes the exact-match handler registered for `url` (which starts with `/`) and
    /// operation `type_`.
    ///
    /// A handler the routes own comes back as its box; a borrowed one (registered by a
    /// [`HandlerRegistration`]) is removed and stays with its owner.
    pub fn drop(&mut self, type_: OperationType, url: &str) -> Option<Box<dyn HandlerBase>> {
        self._map[type_ as usize].remove(url.as_bytes()).and_then(Owned::into_box)
    }

    /// Adds the matching rule `rule` for operation `type_` and returns `self`.
    ///
    /// Rules are searched only when no exact match is found, in the order they were
    /// added: the first added has the highest priority. The routes take ownership of the
    /// rule and drop it when they are dropped.
    #[inline]
    pub fn add_rule(&mut self, rule: Box<MatchRule>, type_: OperationType) -> &mut Self {
        self.add_cookie(rule, type_);
        self
    }

    /// Routes requests for operation `type_` whose path matches `url` to `handler`, and
    /// returns `self`.
    ///
    /// The URL's fixed path must match, followed by its remainder parameter when it has
    /// one; the rule is added as by [`Routes::add_rule`]. For example,
    /// `routes.add(OperationType::Get, Url::new(&"/api".into()).remainder(&"path".into()), handler)`
    /// matches `/api` and everything below it, the rest of the path going to the `path`
    /// parameter. The routes take ownership of the handler and drop it when they are
    /// dropped.
    pub fn add(
        &mut self,
        type_: OperationType,
        url: &Url,
        handler: Box<dyn HandlerBase>,
    ) -> &mut Self {
        let mut rule = Box::new(MatchRule::new(handler));
        rule.add_str(&url._path);
        if !url._param.empty() {
            rule.add_param(&url._param, true);
        }
        self.add_rule(rule, type_)
    }

    /// Sets the default handler, which handles any method and path that no exact match or
    /// rule handles, and returns `self`.
    ///
    /// The routes only borrow the handler.
    ///
    /// # Safety
    /// `handler` stays valid, and unused elsewhere while a request runs it, for as long as
    /// the routes are used.
    pub unsafe fn add_default_handler(&mut self, handler: *mut dyn HandlerBase) -> &mut Self {
        self._default_handler = Some(handler);
        self
    }

    /// Dispatches the request `req` for the path `path`, filling the reply `rep`: the main
    /// entry point of the routes.
    ///
    /// Looks up the handler for the request's method and the path (without a trailing
    /// `/`), with the parameters the match finds in the URL stored in the request. It
    /// checks the handler's mandatory parameters and runs it; an error from either is
    /// turned into a reply by [`Routes::exception_reply`]. When no handler matches, the
    /// reply is a JSON `404 Not Found`.
    pub fn handle(
        &mut self,
        path: &Sstring,
        mut req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        let normalized = Self::normalize_url(path);
        let handler = self.get_handler(str2type(req._method.as_str()), &normalized, &mut req.param);
        let rep = match handler {
            Some(handler) => {
                // SAFETY: the routes own the handler (or the caller keeps the default one
                // alive) for as long as they live.
                let handler = unsafe { &mut *handler };
                match handler.verify_mandatory_params(&req) {
                    Ok(()) => {
                        let r = handler.handle(path, req, rep);
                        // The error path holds the exception handlers (a counter increment),
                        // not the routes, so the routes may go before the reply.
                        let exceptions = std::rc::Rc::clone(&self._exceptions);
                        return r.handle_exception(move |e: ExceptionPtr| {
                            make_ready_future(exception_reply_with(&exceptions, e))
                        });
                    }
                    Err(e) => self.exception_reply(e),
                }
            }
            None => {
                let ex = JsonException::from_base(&not_found_exception("Not found"));
                let mut rep = Box::new(Reply::new());
                rep.set_status(StatusType::NOT_FOUND, json_body(&ex)).done_with("json");
                rep
            }
        };
        make_ready_future(rep)
    }

    /// Returns the exact-match handler for operation `type_` and `url`, or `None` if there
    /// is none.
    #[inline]
    pub fn get_exact_match(
        &self,
        type_: OperationType,
        url: &Sstring,
    ) -> Option<*mut dyn HandlerBase> {
        self._map[type_ as usize].get(url).map(|h| h.ptr)
    }

    /// Returns the handler for operation `type_` and `url`.
    ///
    /// Tries the exact match first, then the rules in insertion order, and falls back to
    /// the default handler (`None` if there is none). The matching rule stores the URL
    /// parameters it finds in `params`; a rule that does not match leaves it empty.
    pub fn get_handler(
        &self,
        type_: OperationType,
        url: &Sstring,
        params: &mut Parameters,
    ) -> Option<*mut dyn HandlerBase> {
        if let Some(handler) = self.get_exact_match(type_, url) {
            return Some(handler);
        }

        for rule in self._rules[type_ as usize].values() {
            // SAFETY: a registered rule is valid while registered (owned, or kept by its
            // RuleRegistration until it unregisters it).
            if let Some(handler) = unsafe { &*rule.ptr }.get(url, params) {
                return Some(handler);
            }
            params.clear();
        }
        self._default_handler
    }

    /// Returns `url` without its trailing `/`, if it has one and is longer than `/`.
    fn normalize_url(url: &Sstring) -> Sstring {
        let b = url.as_bytes();
        if b.len() < 2 || b[b.len() - 1] != b'/' {
            return url.clone();
        }
        Sstring::from_bytes(&b[..b.len() - 1])
    }

    /// Registers an exception handler and returns its id, for
    /// [`Routes::remove_exception_handler`].
    ///
    /// The function receives the error and returns a reply for the errors it handles; any
    /// other error it returns unchanged (or replaced by a new one), and the next handler
    /// receives that error.
    pub fn register_exeption_handler(&mut self, fun: ExceptionHandlerFun) -> ExceptionHandlerId {
        let current = self._exception_id;
        self._exception_id += 1;
        std::rc::Rc::make_mut(&mut self._exceptions).insert(current, std::rc::Rc::new(fun));
        current
    }

    /// Removes an exception handler.
    pub fn remove_exception_handler(&mut self, id: ExceptionHandlerId) {
        std::rc::Rc::make_mut(&mut self._exceptions).remove(&id);
    }

    /// Turns an exception into a json reply.
    pub fn exception_reply(&mut self, eptr: ExceptionPtr) -> Box<Reply> {
        exception_reply_with(&self._exceptions, eptr)
    }

    /// Registers `new_path` as an alias of the already registered `old_path`: a copy of
    /// the handler of `old_path` is registered for `new_path`.
    ///
    /// # Errors
    ///
    /// Returns a runtime error if no handler is registered for `old_path`, if that
    /// handler is not a [`FunctionHandler`], or if registering `new_path` fails.
    pub fn add_alias(
        &mut self,
        old_path: &PathDescription,
        new_path: &PathDescription,
    ) -> Result<(), ExceptionPtr> {
        let mut p = Parameters::default();
        let mut path = old_path.path.clone();
        for p in &old_path.params {
            // the path_description path does not contains the path parameters
            // so just add a fake parameter to the path for each of the parameters,
            // and add the string for each fixed string part.
            if p.type_ == UrlComponentType::FixedString {
                path.append_bytes(p.name.as_bytes());
            } else {
                path.append_bytes(b"/k");
            }
        }
        let a = match self.get_handler(old_path.operations.method, &path, &mut p) {
            None => {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                    "routes::add_alias path_description not found: {}",
                    old_path.path
                ))));
            }
            Some(a) => a,
        };
        // if a handler is found then it must be a function_handler
        // (checked here with a downcast: another handler type is an error).
        // SAFETY: the routes own the handler, alive and not borrowed during these calls.
        let Some(f) = (unsafe { downcast_handler::<FunctionHandler>(a) }) else {
            return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                "routes::add_alias the handler of {} is not a function_handler",
                old_path.path
            ))));
        };
        // SAFETY: as above.
        let f = unsafe { &*f }.clone();
        new_path.set(self, Box::new(f))
    }

    /// Adds the matching rule `rule` for operation `type_` and returns a cookie that
    /// [`Routes::del_cookie`] takes to remove it.
    ///
    /// The routes take ownership of the rule and drop it when they are dropped.
    #[inline]
    pub fn add_cookie(&mut self, rule: Box<MatchRule>, type_: OperationType) -> RuleCookie {
        // SAFETY: the pointer comes from Box::into_raw and is owned by the routes.
        unsafe { self.add_cookie_raw(Box::into_raw(rule), type_, true) }
    }

    /// [`Routes::add_cookie`] with a raw pointer; `owned` as for [`Routes::put_raw`].
    ///
    /// # Safety
    /// As [`Routes::put_raw`], for the rule.
    pub(crate) unsafe fn add_cookie_raw(
        &mut self,
        rule: *mut MatchRule,
        type_: OperationType,
        owned: bool,
    ) -> RuleCookie {
        let pos = self._rover;
        self._rover += 1;
        self._rules[type_ as usize].insert(pos, Owned { ptr: rule, owned });
        pos
    }

    /// Removes the rule of operation `type_` identified by `cookie`, as returned by
    /// [`Routes::add_cookie`].
    ///
    /// A rule the routes own comes back as its box; a borrowed one stays with its owner.
    pub fn del_cookie(
        &mut self,
        cookie: RuleCookie,
        type_: OperationType,
    ) -> Option<Box<MatchRule>> {
        self._rules[type_ as usize].remove(&cookie).and_then(Owned::into_box)
    }
}

// Dropping the routes drops the maps, which frees the match rules and handlers the
// routes own (see `Owned`).

/// The JSON body of an exception reply: the exception's message and code.
#[inline]
fn json_body(j: &JsonException) -> Sstring {
    // Formatting a message and a code cannot fail.
    j.to_json().unwrap_or_default()
}

/// A registration of an exact-match handler in [`Routes`] that unregisters the handler
/// when dropped.
pub struct HandlerRegistration {
    _routes: *mut Routes,
    _url: Sstring,
    _op: OperationType,
}

impl HandlerRegistration {
    /// Registers `h` in the routes `rs` as the exact-match handler of `url` for operation
    /// `op`, as [`Routes::put`] does.
    ///
    /// # Errors
    ///
    /// Returns a runtime error if a handler is already registered for `url` and `op`.
    ///
    /// # Safety
    /// `rs` and `h` stay valid and in place while the registration lives (it keeps raw
    /// pointers to both); the routes borrow `h` and do not free it.
    pub unsafe fn new(
        rs: *mut Routes,
        h: *mut dyn HandlerBase,
        url: &Sstring,
        op: OperationType,
    ) -> Result<Self, ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { (*rs).put_raw(op, url.as_str(), h, false)? };
        Ok(HandlerRegistration { _routes: rs, _url: url.clone(), _op: op })
    }
}

/// Unregisters the handler from the routes with [`Routes::drop`].
impl Drop for HandlerRegistration {
    fn drop(&mut self) {
        // SAFETY: the routes outlive the registration (the constructor's contract); the
        // handler is borrowed, so nothing comes back to free.
        let _ = unsafe { (*self._routes).drop(self._op, self._url.as_str()) };
    }
}

/// A registration of a [`MatchRule`] in [`Routes`] that unregisters the rule when
/// dropped.
pub struct RuleRegistration {
    _routes: *mut Routes,
    _op: OperationType,
    _cookie: RuleCookie,
}

impl RuleRegistration {
    /// Registers `rule` in the routes `rs` for operation `op`, as [`Routes::add_cookie`]
    /// does.
    ///
    /// # Safety
    /// `rs` and `rule` stay valid and in place while the registration lives (it keeps raw
    /// pointers to both); the routes borrow `rule` and do not free it.
    pub unsafe fn new(rs: *mut Routes, rule: *mut MatchRule, op: OperationType) -> Self {
        // SAFETY: the caller's contract.
        let cookie = unsafe { (*rs).add_cookie_raw(rule, op, false) };
        RuleRegistration { _routes: rs, _op: op, _cookie: cookie }
    }
}

/// Unregisters the rule from the routes with [`Routes::del_cookie`].
impl Drop for RuleRegistration {
    fn drop(&mut self) {
        // SAFETY: the routes outlive the registration (the constructor's contract); the rule
        // is borrowed, so nothing comes back to free.
        let _ = unsafe { (*self._routes).del_cookie(self._cookie, self._op) };
    }
}
