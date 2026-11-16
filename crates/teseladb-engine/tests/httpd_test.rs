//! Tests of the HTTP server and client: routing and URL matching, request and response
//! parsing, chunked bodies and trailers, JSON streaming, keep-alive and connection limits,
//! client retries and errors, content length limits, and the server over loopback sockets.
//! Objects shared by reference are captured as raw pointers, valid because the owner
//! outlives the thread that uses them; an expected failure is checked by the type of the
//! returned `ExceptionPtr`.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use loopback_socket::{LoopbackConnectionFactory, LoopbackSocketImpl};
use teseladb::engine::core::abort_source::{AbortRequestedException, AbortSource};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, InvalidArgument, OutOfRange, Promise, RuntimeError, SystemError,
    make_exception_future, make_ready_future,
};
use teseladb::engine::core::future_util::sleep_abortable_with;
use teseladb::engine::core::iostream::{
    DataSink, DataSinkImpl, InputStream, OutputStream, OutputStreamOptions, OutputStreamState,
};
use teseladb::engine::core::loop_::{StopIteration, do_for_each, do_until, repeat};
#[cfg(feature = "tls")]
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::reactor::yield_;
#[cfg(feature = "tls")]
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::shared_future::SharedPromise;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::{Sstring, to_sstring, to_sstring_buffer};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::timer::SteadyClockType;
use teseladb::engine::core::when_all::when_all;
use teseladb::engine::http::client::experimental::{Client, ReplyHandler, RetryRequests};
use teseladb::engine::http::common::httpd::{OperationType, Parameters};
use teseladb::engine::http::connection_factory::experimental::ConnectionFactory;
use teseladb::engine::http::exception::{
    BaseException, ExceptionKind, redirect_exception, redirect_exception_default,
    server_error_exception, unexpected_status_error,
};
use teseladb::engine::http::file_handler::FileTransformer;
use teseladb::engine::http::function_handlers::{FunctionHandler, FutureHandlerFunction};
use teseladb::engine::http::handlers::{ConstReq, HandlerBase, HandlerBaseData};
use teseladb::engine::http::httpd::httpd::{HttpServer, HttpServerTester};
use teseladb::engine::http::json_path::{PathDescription, PathPart, UrlComponentType};
use teseladb::engine::http::matcher::{Matcher, ParamMatcher, StrMatcher};
use teseladb::engine::http::matchrules::MatchRule;
use teseladb::engine::http::reply::{BodyWriter, Reply, StatusClass, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine::http::response_parser::HttpResponseParser;
use teseladb::engine::http::routes::{HandlerRegistration, Routes, RuleRegistration, Url};
use teseladb::engine::http::transformers::ContentReplace;
use teseladb::engine::http::url::internal as url_internal;
use teseladb::engine::json::formatter::{JsonWrite, ToJson};
use teseladb::engine::json::json_elements::{
    JsonBase, JsonBaseElement, JsonElement, JsonReturnType, JsonVoid, Jsonable, stream_object,
};
use teseladb::engine::net::api::{AcceptResult, ConnectedSocket, make_ipv4_address};
#[cfg(feature = "tls")]
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::net::stack::SocketImpl;
#[cfg(feature = "tls")]
use teseladb::engine::net::tls;
use teseladb::engine::util::closeable::{Closeable, DeferredClose};
use teseladb::engine::util::short_streams::util;
use teseladb::engine::util::string_utils::internal::CaseInsensitiveKey;

const EPIPE: i32 = 32;
#[cfg(target_os = "linux")]
const ECONNABORTED: i32 = 103;
#[cfg(not(target_os = "linux"))]
const ECONNABORTED: i32 = 53;
#[cfg(target_os = "linux")]
const ENOBUFS: i32 = 105;
#[cfg(not(target_os = "linux"))]
const ENOBUFS: i32 = 55;

/// "Not found", for the results of [`find`].
const NPOS: Option<usize> = None;

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

/// The IPv4 wildcard address with port 0.
fn any_addr() -> SocketAddress {
    make_ipv4_address(&Ipv4Addr::default())
}

/// The position of the first `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The error number of a system error (the library raises it as `std::io::Error`
/// or as `SystemError`).
fn system_error_code(e: &ExceptionPtr) -> Option<i32> {
    if let Some(err) = e.downcast_ref::<std::io::Error>() {
        return err.raw_os_error();
    }
    e.downcast_ref::<SystemError>().map(|err| err.code())
}

/// Whether `e` is a system error; gnutls errors count as system errors too.
fn is_system_error(e: &ExceptionPtr) -> bool {
    system_error_code(e).is_some() || is_tls_error(e)
}

/// Whether `e` is a gnutls error.
#[cfg(feature = "tls")]
fn is_tls_error(e: &ExceptionPtr) -> bool {
    e.is::<tls::TlsError>()
}

/// Without the feature `tls` there are no gnutls errors.
#[cfg(not(feature = "tls"))]
fn is_tls_error(_e: &ExceptionPtr) -> bool {
    false
}

/// Whether `e` is a runtime error, or one of the library errors built on it.
fn is_runtime_error(e: &ExceptionPtr) -> bool {
    e.is::<RuntimeError>() || is_system_error(e) || e.is::<BaseException>()
}

fn http_exception_kind(e: &ExceptionPtr) -> Option<ExceptionKind> {
    e.downcast_ref::<BaseException>().map(|b| b.kind().clone())
}

/// A `new handler` the routes take ownership of.
fn raw_handler<H: HandlerBase + 'static>(h: H) -> Box<dyn HandlerBase> {
    Box::new(h)
}

/// Whether the handler found is `h`.
fn same_handler<H>(res: Option<*mut dyn HandlerBase>, h: *const H) -> bool {
    match res {
        Some(p) => std::ptr::addr_eq(p, h),
        None => h.is_null(),
    }
}

/// The reply handler of `Client::make_request` from a closure on the reply and its body stream.
fn reply_handler<F>(f: F) -> ReplyHandler
where
    F: FnMut(&Reply, InputStream<u8>) -> Future<()> + 'static,
{
    ReplyHandler::new(f)
}

macro_rules! handler_base_data {
    () => {
        fn base(&self) -> &HandlerBaseData {
            &self.base
        }
        fn base_mut(&mut self) -> &mut HandlerBaseData {
            &mut self.base
        }
    };
}

#[derive(Default)]
struct Handl {
    base: HandlerBaseData,
}

impl HandlerBase for Handl {
    handler_base_data!();

    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        rep.done_with("html");
        make_ready_future(rep)
    }
}

/// An HTTP client connection factory over the loopback socket factory.
struct LoopbackHttpFactory {
    lsi: LoopbackSocketImpl,
}

impl LoopbackHttpFactory {
    fn new(f: &mut LoopbackConnectionFactory) -> Self {
        LoopbackHttpFactory { lsi: LoopbackSocketImpl::new(f, None) }
    }
}

impl ConnectionFactory for LoopbackHttpFactory {
    fn make(&mut self, _as: *mut AbortSource) -> Future<ConnectedSocket> {
        self.lsi.connect(any_addr(), any_addr(), Transport::TCP)
    }
}

/// An HTTP client over the loopback factory `lcf`, with a connection limit and retry policy.
///
/// The client is pinned in its box: its connections point at it.
fn loopback_client(
    lcf: *mut LoopbackConnectionFactory,
    max_connections: u32,
    retry: RetryRequests,
) -> Pin<Box<Client>> {
    // SAFETY: the factory outlives the client.
    let f = LoopbackHttpFactory::new(unsafe { &mut *lcf });
    Box::pin(Client::with_factory(
        Box::new(f),
        max_connections,
        retry,
        Client::DEFAULT_MAX_BYTES_TO_DRAIN,
    ))
}

/// An HTTP client over the loopback factory `lcf`, with the default limits.
fn default_loopback_client(lcf: *mut LoopbackConnectionFactory) -> Pin<Box<Client>> {
    loopback_client(lcf, Client::DEFAULT_MAX_CONNECTIONS, RetryRequests::NO)
}

/// An HTTP server pinned in its box, where its setup runs.
struct TestServer(Pin<Box<HttpServer>>);

impl TestServer {
    fn new(name: &Sstring) -> Self {
        let mut s = Box::pin(HttpServer::new(name));
        s.as_mut().start();
        TestServer(s)
    }

    /// The server, for one call that does not move it.
    fn get(&mut self) -> &mut HttpServer {
        // SAFETY: every caller uses the reference for one method call or field access, none
        // of which moves the server out of its box.
        unsafe { self.0.as_mut().get_unchecked_mut() }
    }

    fn pin(&mut self) -> Pin<&mut HttpServer> {
        self.0.as_mut()
    }
}

/// A JSON body writer as the body writer function the reply takes.
fn json_body(mut bw: teseladb::engine::json::json_elements::BodyWriter) -> BodyWriter {
    BodyWriter::new(move |s: OutputStream<u8>| bw(s))
}

/// `deferred_close` of an output stream, through the pointer the test keeps writing with.
struct StreamCloser(*mut OutputStream<u8>);

impl Closeable for StreamCloser {
    fn close(&mut self) -> Future<()> {
        // SAFETY: the stream outlives the guard that closes it (it is declared before it).
        unsafe { (*self.0).close() }
    }
}

fn get_request(path: &str) -> Request {
    Request::make(ss("GET"), ss("test"), ss(path))
}

teseladb::engine_test_case!(test_reply, {
    let mut r = Reply::new();
    r.set_content_type("txt");
    assert_eq!(
        r._headers.get(&CaseInsensitiveKey(ss("Content-Type"))).cloned().unwrap_or_default(),
        ss("text/plain")
    );
    make_ready_future(())
});

teseladb::engine_test_case!(test_str_matcher, {
    let m = StrMatcher::new(&ss("/hello"));
    let mut param = Parameters::default();
    assert_eq!(m.match_(&ss("/abc/hello"), 4, &mut param), 10usize);
    make_ready_future(())
});

teseladb::engine_test_case!(test_param_matcher, {
    let m = ParamMatcher::new(&ss("param"), false);
    let mut param = Parameters::default();
    assert_eq!(m.match_(&ss("/abc/hello"), 4, &mut param), 10usize);
    assert_eq!(param.path(&ss("param")).cloned().unwrap_or_default(), "/hello");
    assert_eq!(param.get_decoded_param(&ss("param")), "hello");
    make_ready_future(())
});

teseladb::engine_test_case!(test_match_rule, {
    let mut param = Parameters::default();
    let mut hb = Box::new(Handl::default());
    let h: *mut Handl = &mut *hb;
    let mut mr = MatchRule::new(hb);
    mr.add_str(&ss("/hello")).add_param(&ss("param"), false);
    let mut res = mr.get(&ss("/hello/val1"), &mut param);
    assert!(same_handler(res, h));
    assert_eq!(param.get_decoded_param(&ss("param")), "val1");
    res = mr.get(&ss("/hell/val1"), &mut param);
    let nl: *const Handl = std::ptr::null();
    assert!(same_handler(res, nl));

    // Test that URL decoding happens correctly on the path part of
    // the URL.
    let _ = mr.get(&ss("/hello/hello%21hi"), &mut param);
    assert_eq!(param.get_decoded_param(&ss("param")), "hello!hi");

    make_ready_future(())
});

teseladb::engine_test_case!(test_match_rule_order, {
    let mut param = Parameters::default();
    let mut route = Routes::new();

    let mut h1 = Box::new(Handl::default());
    let h1p: *mut Handl = &mut *h1;
    route.add(OperationType::Get, &Url::new(&ss("/hello")), h1);

    let h2 = Box::new(Handl::default());
    route.add(OperationType::Get, &Url::new(&ss("/hello")), h2);

    let rh = route.get_handler(OperationType::Get, &ss("/hello"), &mut param);
    assert!(same_handler(rh, h1p));

    make_ready_future(())
});

teseladb::engine_test_case!(test_put_drop_rule, {
    let mut rts = Routes::new();
    let mut h = Box::new(Handl::default());
    let hp: *mut Handl = &mut *h;
    let mut params = Parameters::default();

    {
        // SAFETY: the routes and the handler outlive the registration, dropped at the end
        // of this block.
        let _reg = unsafe {
            HandlerRegistration::new(
                &mut rts,
                hp as *mut dyn HandlerBase,
                &ss("/hello"),
                OperationType::Get,
            )
        }
        .expect("handler_registration");
        let res = rts.get_handler(OperationType::Get, &ss("/hello"), &mut params);
        assert!(same_handler(res, hp));
    }

    let res = rts.get_handler(OperationType::Get, &ss("/hello"), &mut params);
    let nl: *const Handl = std::ptr::null();
    assert!(same_handler(res, nl));
    drop(h);
    make_ready_future(())
});

// Putting a duplicated exact rule would result
// in a memory leak due to the fact that rules are implemented
// as raw pointers. In order to prevent such leaks,
// an exception is thrown if somebody tries to put
// a duplicated rule without removing the old one first.
// The interface demands that the callee allocates the handle,
// so it should also expect the callee to free it before
// overwriting.
teseladb::engine_test_case!(test_duplicated_exact_rule, {
    let _param = Parameters::default();
    let mut route = Routes::new();

    let h1 = raw_handler(Handl::default());
    route.put(OperationType::Get, "/hello", h1).expect("put");

    let h2 = raw_handler(Handl::default());
    let r = route.put(OperationType::Get, "/hello", h2).map(|_| ());
    assert!(r.is_err_and(|e| e.is::<RuntimeError>()));

    // `delete route.drop(GET, "/hello")`: the routes hand the handler back, to be freed.
    drop(route.drop(OperationType::Get, "/hello"));
    // The refused `put` above freed `h2`, so the same
    // handler is put again as a new one.
    let h2 = raw_handler(Handl::default());
    route.put(OperationType::Get, "/hello", h2).expect("put");

    make_ready_future(())
});

teseladb::engine_test_case!(test_add_del_cookie, {
    let mut rts = Routes::new();
    let mut hb = Box::new(Handl::default());
    let h: *mut Handl = &mut *hb;
    let mut mr = MatchRule::new(hb);
    mr.add_str(&ss("/hello"));
    let mut params = Parameters::default();

    {
        // SAFETY: the routes and the rule outlive the registration, dropped at the end of
        // this block.
        let _reg = unsafe { RuleRegistration::new(&mut rts, &mut mr, OperationType::Get) };
        let res = rts.get_handler(OperationType::Get, &ss("/hello"), &mut params);
        assert!(same_handler(res, h));
    }

    let res = rts.get_handler(OperationType::Get, &ss("/hello"), &mut params);
    let nl: *const Handl = std::ptr::null();
    assert!(same_handler(res, nl));
    make_ready_future(())
});

fn to_json<T: ToJson + ?Sized>(v: &T) -> Result<Sstring, ExceptionPtr> {
    v.to_json()
}

teseladb::engine_test_case!(test_formatter, {
    assert_eq!(to_json(&true).expect("to_json"), "true");
    assert_eq!(to_json(&false).expect("to_json"), "false");
    assert_eq!(to_json(&1i32).expect("to_json"), "1");
    let txt: &str = "efg";
    assert_eq!(to_json(txt).expect("to_json"), "\"efg\"");
    let str = ss("abc");
    assert_eq!(to_json(&str).expect("to_json"), "\"abc\"");
    let mut f: f32 = 1.0;
    assert_eq!(to_json(&f).expect("to_json"), "1");
    f = f32::INFINITY;
    assert!(to_json(&f).is_err_and(|e| e.is::<OutOfRange>()));
    f = f32::NEG_INFINITY;
    assert!(to_json(&f).is_err_and(|e| e.is::<OutOfRange>()));
    f = f32::NAN;
    assert!(to_json(&f).is_err_and(|e| e.is::<InvalidArgument>()));
    let mut d: f64 = -1.0;
    assert_eq!(to_json(&d).expect("to_json"), "-1");
    d = f64::INFINITY;
    assert!(to_json(&d).is_err_and(|e| e.is::<OutOfRange>()));
    d = f64::NEG_INFINITY;
    assert!(to_json(&d).is_err_and(|e| e.is::<OutOfRange>()));
    d = f64::NAN;
    assert!(to_json(&d).is_err_and(|e| e.is::<InvalidArgument>()));
    make_ready_future(())
});

teseladb::engine_test_case!(test_decode_url, {
    let mut req = Request { _url: ss("/a?q=%23%24%23"), ..Request::default() };
    let url = req.parse_query_param();
    assert_eq!(url, "/a");
    assert_eq!(req.get_query_param("q"), "#$#");
    req._url = ss("/a?a=%23%24%23&b=%22%26%22");
    req.parse_query_param();
    assert_eq!(req.get_query_param("a"), "#$#");
    assert_eq!(req.get_query_param("b"), "\"&\"");
    make_ready_future(())
});

teseladb::engine_test_case!(test_decode_path, {
    let mut req = Request { param: Parameters::default(), ..Request::default() };
    req.param.set(&ss("param1"), &ss("/a+b"));
    req.param.set(&ss("param2"), &ss("/same%2Ba%2Bb"));
    req.param.set(&ss("param3"), &ss("/another_param"));
    req.param.set(&ss("param4"), &ss("/yet%20another"));
    req.param.set(&ss("invalid_param"), &ss("/%2"));

    assert_eq!(req.get_path_param(&ss("param1")), "a+b");
    assert_eq!(req.get_path_param(&ss("param2")), "same+a+b");
    assert_eq!(req.get_path_param(&ss("param3")), "another_param");
    assert_eq!(req.get_path_param(&ss("param4")), "yet another");
    assert_eq!(req.get_path_param(&ss("invalid_param")), "");
    assert_eq!(req.get_path_param(&ss("missing_param")), "");
    make_ready_future(())
});

teseladb::engine_test_case!(test_routes, {
    let h1 = Box::new(Handl::default());
    let h2 = Box::new(Handl::default());
    // The routes outlive the four handle() calls: they stay alive until
    // the returned when_all() is built, and every handler here resolves immediately.
    let mut route = Box::new(Routes::new());
    let mut api = Url::new(&ss("/api"));
    api.remainder(&ss("path"));
    route.add(OperationType::Get, &api, h1);
    route.add(OperationType::Get, &Url::new(&ss("/")), h2);
    let mut req = Box::new(Request::default());
    let mut rep = Box::new(Reply::new());

    let f1 = route.handle(&ss("/api"), req, rep).then(|rep: Box<Reply>| {
        assert_eq!(rep._status.0, StatusType::OK.0);
    });
    req = Box::new(Request::default());
    rep = Box::new(Reply::new());

    let f2 = route.handle(&ss("/"), req, rep).then(|rep: Box<Reply>| {
        assert_eq!(rep._status.0, StatusType::OK.0);
    });
    req = Box::new(Request::default());
    rep = Box::new(Reply::new());
    let f3 = route.handle(&ss("/api/abc"), req, rep).then(|_rep: Box<Reply>| {});
    req = Box::new(Request::default());
    rep = Box::new(Reply::new());
    let f4 = route.handle(&ss("/ap"), req, rep).then(|rep: Box<Reply>| {
        assert_eq!(rep._status.0, StatusType::NOT_FOUND.0);
    });
    when_all((f1, f2, f3, f4)).then(
        move |fs: (Future<()>, Future<()>, Future<()>, Future<()>)| -> Result<(), ExceptionPtr> {
            let _route = route;
            fs.0.get()?;
            fs.1.get()?;
            fs.2.get()?;
            fs.3.get()?;
            Ok(())
        },
    )
});

teseladb::engine_test_case!(routes_reply_outlives_routes, {
    // The futures of Routes::handle hold what they still need (the handler's content
    // type, the exception handlers) instead of pointing back at the routes, so the routes
    // may go before the replies are done.
    let (ok, failed) = {
        let mut route = Routes::new();
        let slow: FutureHandlerFunction = Rc::new(|_req: Box<Request>, rep: Box<Reply>| {
            yield_().then(move |()| make_ready_future(rep))
        });
        let failing: FutureHandlerFunction = Rc::new(|_req: Box<Request>, _rep: Box<Reply>| {
            yield_().then(|()| {
                make_exception_future::<Box<Reply>>(ExceptionPtr::new(RuntimeError::new(
                    "late failure".to_string(),
                )))
            })
        });
        route
            .put(
                OperationType::Get,
                "/slow",
                Box::new(FunctionHandler::from_future_handler_function(slow, &ss("txt"))),
            )
            .expect("put");
        route
            .put(
                OperationType::Get,
                "/failing",
                Box::new(FunctionHandler::from_future_handler_function(failing, &ss("txt"))),
            )
            .expect("put");
        let ok = route.handle(&ss("/slow"), Box::new(Request::default()), Box::new(Reply::new()));
        let failed =
            route.handle(&ss("/failing"), Box::new(Request::default()), Box::new(Reply::new()));
        (ok, failed)
    };
    when_all((ok, failed)).then(
        |fs: (Future<Box<Reply>>, Future<Box<Reply>>)| -> Result<(), ExceptionPtr> {
            let ok = fs.0.get()?;
            assert_eq!(ok._status, StatusType::OK);
            assert_eq!(ok.get_header("Content-Type"), "text/plain");
            let failed = fs.1.get()?;
            assert_eq!(failed._status, StatusType::INTERNAL_SERVER_ERROR);
            Ok(())
        },
    )
});

teseladb::engine_test_case!(test_json_path, {
    let res1 = Rc::new(Cell::new(false));
    let res2 = Rc::new(Cell::new(false));
    let res3 = Rc::new(Cell::new(false));
    let res4 = Rc::new(Cell::new(false));
    let route: Rc<RefCell<Routes>> = Rc::new(RefCell::new(Routes::new()));
    let part = |name: &str, type_: UrlComponentType| PathPart { name: ss(name), type_ };
    let path1 = PathDescription::with_parts(
        &ss("/my/path"),
        OperationType::Get,
        &ss("path1"),
        &[part("param1", UrlComponentType::Param), part("/text", UrlComponentType::FixedString)],
        &[],
    );
    let path2 = PathDescription::with_parts(
        &ss("/my/path"),
        OperationType::Get,
        &ss("path2"),
        &[part("param1", UrlComponentType::Param), part("param2", UrlComponentType::Param)],
        &[],
    );
    let path3 = PathDescription::with_parts(
        &ss("/my/path"),
        OperationType::Get,
        &ss("path3"),
        &[
            part("param1", UrlComponentType::Param),
            part("param2", UrlComponentType::ParamUntilEndOfPath),
        ],
        &[],
    );
    let path4 = PathDescription::with_parts(
        &ss("/double/encoded"),
        OperationType::Get,
        &ss("path4"),
        &[part("param1", UrlComponentType::ParamUntilEndOfPath)],
        &[],
    );

    let r1 = res1.clone();
    path1
        .set_json_request_function(
            &mut route.borrow_mut(),
            Box::new(move |req: ConstReq<'_>| {
                r1.set(true);
                assert_eq!(req.get_path_param(&ss("param1")), "value1");
                JsonReturnType::new("")
            }),
        )
        .expect("set");

    let r2 = res2.clone();
    path2
        .set_json_request_function(
            &mut route.borrow_mut(),
            Box::new(move |req: ConstReq<'_>| {
                r2.set(true);
                assert_eq!(req.get_path_param(&ss("param1")), "value4+value4 value4");

                assert_eq!(req.get_path_param(&ss("param2")), "text4+text4");
                JsonReturnType::new("")
            }),
        )
        .expect("set");

    let r3 = res3.clone();
    path3
        .set_json_request_function(
            &mut route.borrow_mut(),
            Box::new(move |req: ConstReq<'_>| {
                r3.set(true);
                assert_eq!(req.get_path_param(&ss("param1")), "value3");

                assert_eq!(req.get_path_param(&ss("param2")), "text2/text3");
                JsonReturnType::new("")
            }),
        )
        .expect("set");

    let r4 = res4.clone();
    path4
        .set_json_request_function(
            &mut route.borrow_mut(),
            Box::new(move |req: ConstReq<'_>| {
                r4.set(true);
                assert_eq!(req.get_path_param(&ss("param1")), "example%20");
                JsonReturnType::new("")
            }),
        )
        .expect("set");

    let check_handler = {
        let route = route.clone();
        move |raw_url: &str, res: Rc<Cell<bool>>| -> Future<()> {
            let mut req = Request { _url: ss(raw_url), ..Request::default() };
            let url = req.parse_query_param();
            let route2 = route.clone();
            let f = route.borrow_mut().handle(
                &url,
                Box::new(Request::default()),
                Box::new(Reply::new()),
            );
            f.then(move |_f: Box<Reply>| {
                let _route = route2;
                assert!(res.get());
            })
        }
    };

    let f1 = check_handler("/my/path/value1/text", res1);
    let f2 = check_handler("/my/path/value4+value4%20value4/text4%2Btext4", res2);
    let f3 = check_handler("/my/path/value3/text2/text3", res3);
    let f4 = check_handler("/double/encoded/example%2520", res4);

    when_all((f1, f2, f3, f4)).then(
        |fs: (Future<()>, Future<()>, Future<()>, Future<()>)| -> Result<(), ExceptionPtr> {
            fs.0.get()?;
            fs.1.get()?;
            fs.2.get()?;
            fs.3.get()?;
            Ok(())
        },
    )
});

/**
 * \brief a helper data sink that stores everything it gets in a stringstream
 */
struct MemoryDataSinkImpl {
    _ss: Rc<RefCell<Vec<u8>>>,
}

impl MemoryDataSinkImpl {
    fn new(ss: Rc<RefCell<Vec<u8>>>) -> Self {
        MemoryDataSinkImpl { _ss: ss }
    }
}

impl DataSinkImpl for MemoryDataSinkImpl {
    fn put(&mut self, _data: Packet) -> Future<()> {
        std::process::abort();
    }
    fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
        self._ss.borrow_mut().extend_from_slice(buf.as_slice());
        make_ready_future(())
    }
    fn flush(&mut self) -> Future<()> {
        make_ready_future(())
    }

    fn close(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

/// A data sink that appends what it receives to `ss`.
fn memory_data_sink(ss: Rc<RefCell<Vec<u8>>>) -> DataSink {
    DataSink::new(Box::new(MemoryDataSinkImpl::new(ss)))
}

fn ss_str(ss: &Rc<RefCell<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&ss.borrow()).into_owned()
}

fn test_transformer_stream(
    ss: Rc<RefCell<Vec<u8>>>,
    cr: *mut ContentReplace,
    buffer_parts: Vec<Sstring>,
) -> Future<()> {
    let mut req = Box::new(Request::default());
    ss.borrow_mut().clear();
    req._headers.insert(CaseInsensitiveKey(ss_("Host")), ss_("localhost"));
    let opts = OutputStreamOptions { trim_to_size: true, ..Default::default() };
    // SAFETY: the content_replace outlives the stream.
    let os = unsafe {
        (*cr).transform(
            req,
            &ss_("json"),
            OutputStream::with_options(memory_data_sink(ss), 32000, opts),
        )
    };
    do_with((os, buffer_parts), |st: &mut (OutputStream<u8>, Vec<Sstring>)| {
        let osp: *mut OutputStream<u8> = &mut st.0;
        let pp: *const Vec<Sstring> = &st.1;
        let n = st.1.len();
        // SAFETY: do_with keeps the stream and the parts alive.
        do_for_each(0..n, move |i: usize| unsafe { (*osp).write_sstring(&(&*pp)[i]) })
            .then(move |()| unsafe { (*osp).close() })
    })
}

/// `sstring` from a literal, for the transformer tests where `ss` names the stream.
fn ss_(s: &str) -> Sstring {
    Sstring::from(s)
}

teseladb::engine_test_case!(test_transformer, {
    do_with((Rc::new(RefCell::new(Vec::<u8>::new())), ContentReplace::new(&ss("json"))), |st| {
        let ss = st.0.clone();
        let cr: *mut ContentReplace = &mut st.1;
        let opts = OutputStreamOptions { trim_to_size: true, ..Default::default() };
        // SAFETY: do_with keeps the content_replace alive for the chain.
        let os = unsafe {
            (*cr).transform(
                Box::new(Request::default()),
                &ss_("html"),
                OutputStream::with_options(memory_data_sink(ss.clone()), 32000, opts),
            )
        };
        do_with(os, |os: &mut OutputStream<u8>| {
            let osp: *mut OutputStream<u8> = os;
            // SAFETY: do_with keeps the stream alive.
            os.write_sstring(&ss_("hello-{{Protocol}}-xyz-{{Host}}"))
                .then(move |()| unsafe { (*osp).close() })
        })
        .then(move |()| {
            assert_eq!(ss_str(&ss), "hello-{{Protocol}}-xyz-{{Host}}");
            let parts = |v: &[&str]| v.iter().map(|s| ss_(s)).collect::<Vec<Sstring>>();
            test_transformer_stream(
                ss.clone(),
                cr,
                parts(&["hell", "o-{", "{Pro", "tocol}}-xyz-{{Ho", "st}}{{Pr"]),
            )
            .then(move |()| {
                assert_eq!(ss_str(&ss), "hello-http-xyz-localhost{{Pr");
                test_transformer_stream(
                    ss.clone(),
                    cr,
                    parts(&[
                        "hell",
                        "o-{{",
                        "Pro",
                        "tocol}}{{Protocol}}-{{Protoxyz-{{Ho",
                        "st}}{{Pr",
                    ]),
                )
                .then(move |()| {
                    assert_eq!(ss_str(&ss), "hello-httphttp-{{Protoxyz-localhost{{Pr");
                    test_transformer_stream(
                        ss.clone(),
                        cr,
                        parts(&["hell", "o-{{Pro", "t{{Protocol}}ocol}}", "{{Host}}"]),
                    )
                    .then(move |()| {
                        assert_eq!(ss_str(&ss), "hello-{{Prothttpocol}}localhost");
                    })
                })
            })
        })
    })
});

/// Parses a hexadecimal number: leading blanks, an optional sign, then hex digits.
fn stoi16(s: &[u8]) -> u32 {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = i < s.len() && s[i] == b'-';
    if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
        i += 1;
    }
    let start = i;
    let mut v: i64 = 0;
    while i < s.len() && s[i].is_ascii_hexdigit() {
        v = v * 16 + (s[i] as char).to_digit(16).map_or(0, i64::from);
        i += 1;
    }
    assert!(i > start, "stoi: no conversion");
    (if neg { -v } else { v }) as i32 as u32
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HttpConsumerStatus {
    ReadingHeaders,
    ChunkSize,
    ChunkBody,
    ChunkEnd,
    ReadingBodyBySize,
    Done,
}

/// Consumes an HTTP response stream and records its headers and body.
struct HttpConsumer {
    _headers: BTreeMap<Vec<u8>, Vec<u8>>,
    _body: Vec<u8>,
    _remain: u32,
    _current: Vec<u8>,
    last: Option<u8>,
    _size: u32,
    _concat: bool,
    status: HttpConsumerStatus,
}

impl Default for HttpConsumer {
    fn default() -> Self {
        HttpConsumer {
            _headers: BTreeMap::new(),
            _body: Vec::new(),
            _remain: 0,
            _current: Vec::new(),
            last: None,
            _size: 0,
            _concat: true,
            status: HttpConsumerStatus::ReadingHeaders,
        }
    }
}

impl HttpConsumer {
    fn read(&mut self, b: &TemporaryBuffer<u8>) -> bool {
        use HttpConsumerStatus as S;
        for &c in b.as_slice() {
            if self.last == Some(b'\r') && c == b'\n' {
                if self._current.is_empty() {
                    if self.status == S::ReadingHeaders
                        || (self.status == S::ChunkBody && self._remain == 0)
                    {
                        if self.status == S::ReadingHeaders
                            && self._headers.contains_key(&b"Content-Length"[..])
                        {
                            self._remain = stoi16(&self._headers[&b"Content-Length"[..]]);
                            if self._remain == 0 {
                                self.status = S::Done;
                                break;
                            }
                            self.status = S::ReadingBodyBySize;
                        } else {
                            self.status = S::ChunkSize;
                        }
                    } else if self.status == S::ChunkEnd {
                        self.status = S::Done;
                        break;
                    }
                } else {
                    match self.status {
                        S::ReadingHeaders => {
                            let cur = std::mem::take(&mut self._current);
                            self.add_header(&cur);
                        }
                        S::ChunkSize => {
                            let cur = std::mem::take(&mut self._current);
                            self.set_chunk(&cur);
                        }
                        _ => {}
                    }
                    self._current.clear();
                }
                self.last = None;
            } else {
                if let Some(l) = self.last {
                    if self.status == S::ChunkBody || self.status == S::ReadingBodyBySize {
                        if self._concat {
                            self._body.push(l);
                        }
                        self._size += 1;
                        self._remain = self._remain.wrapping_sub(1);
                        if self._remain <= 1 && self.status == S::ReadingBodyBySize {
                            if self._concat {
                                self._body.push(c);
                            }
                            self._size += 1;
                            self.status = S::Done;
                            break;
                        }
                    } else {
                        self._current.push(l);
                    }
                }
                self.last = Some(c);
            }
        }
        self.status == S::Done
    }

    fn set_chunk(&mut self, s: &[u8]) {
        self._remain = stoi16(s);
        if self._remain == 0 {
            self.status = HttpConsumerStatus::ChunkEnd;
        } else {
            self.status = HttpConsumerStatus::ChunkBody;
        }
    }

    fn add_header(&mut self, s: &[u8]) {
        let strs: Vec<&[u8]> = s.split(|&c| c == b':').collect();
        if strs.len() > 1 {
            self._headers.insert(strs[0].to_vec(), strs[1].to_vec());
        }
    }
}

type WriteFunc = BodyWriter;

/// The handler that `test_client_server::run_test` registers.
struct RunTestHandler {
    base: HandlerBaseData,
    count: usize,
    _server: *mut HttpServer,
    _write_func: Option<WriteFunc>,
    _all_message_sent: Promise<()>,
}

impl RunTestHandler {
    fn new(server: *mut HttpServer, write_func: WriteFunc) -> Self {
        RunTestHandler {
            base: HandlerBaseData::default(),
            count: 0,
            _server: server,
            _write_func: Some(write_func),
            _all_message_sent: Promise::new(),
        }
    }
    fn wait_for_message(&mut self) -> Future<()> {
        self._all_message_sent.get_future()
    }
}

impl HandlerBase for RunTestHandler {
    handler_base_data!();

    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        // Take the function: a second call gets an empty one.
        rep.write_body_writer("json", self._write_func.take().unwrap_or_default());
        self.count += 1;
        self._all_message_sent.set_value(());
        make_ready_future(rep)
    }
}

/// The handler that `test_client_server::run` registers.
struct RunHandler {
    base: HandlerBaseData,
    count: usize,
    _server: *mut HttpServer,
    _tests: Vec<(bool, usize)>,
    _all_message_sent: Promise<()>,
}

impl RunHandler {
    fn new(server: *mut HttpServer, tests: &[(bool, usize)]) -> Self {
        RunHandler {
            base: HandlerBaseData::default(),
            count: 0,
            _server: server,
            _tests: tests.to_vec(),
            _all_message_sent: Promise::new(),
        }
    }
    fn wait_for_message(&mut self) -> Future<()> {
        self._all_message_sent.get_future()
    }
}

impl HandlerBase for RunHandler {
    handler_base_data!();

    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        rep.write_body_writer(
            "txt",
            TestClientServer::make_writer(self._tests[self.count].1, self._tests[self.count].0),
        );
        self.count += 1;
        if self.count == self._tests.len() {
            self._all_message_sent.set_value(());
        }
        make_ready_future(rep)
    }
}

/// A server and a client over a connected socket pair for the body writer tests.
struct TestClientServer;

impl TestClientServer {
    fn write_request(output: &mut OutputStream<u8>) -> Future<()> {
        let op: *mut OutputStream<u8> = output;
        // SAFETY: the caller keeps the stream alive until the future resolves.
        output
            .write_sstring(&ss("GET /test HTTP/1.1\r\nHost: myhost.org\r\n\r\n"))
            .then(move |()| unsafe { (*op).flush() })
    }

    /// Reads one reply into `htp`.
    fn read_reply(input: *mut InputStream<u8>, htp: *mut HttpConsumer) -> Future<()> {
        repeat(move || {
            // SAFETY: the caller keeps the stream and the consumer alive until the loop ends.
            unsafe { (*input).read() }.then(move |b: TemporaryBuffer<u8>| {
                // SAFETY: the caller keeps the consumer alive until the loop ends, and nothing else references it meanwhile.
                if b.size() == 0 || unsafe { (*htp).read(&b) } {
                    make_ready_future(StopIteration::YES)
                } else {
                    make_ready_future(StopIteration::NO)
                }
            })
        })
    }

    fn run_test(
        write_func: WriteFunc,
        reader: impl Fn(usize, &mut HttpConsumer) -> bool + 'static,
    ) -> Future<()> {
        do_with((LoopbackConnectionFactory::new(1), HttpServer::new(&ss("test"))), move |st| {
            let lcf: *mut LoopbackConnectionFactory = &mut st.0;
            let server: *mut HttpServer = &mut st.1;
            // SAFETY: do_with keeps the server in place until it stops: the constructor body
            // runs there.
            unsafe { Pin::new_unchecked(&mut *server) }.start();
            // SAFETY: the do_with()s keep the factory,
            // the server and the socket alive until the server stops.
            do_with(
                LoopbackSocketImpl::new(unsafe { &mut *lcf }, None),
                move |lsi: &mut LoopbackSocketImpl| {
                    let lsi: *mut LoopbackSocketImpl = lsi;
                    // SAFETY: the outer do_with keeps the factory and the server alive until the server stops.
                    HttpServerTester::listeners(unsafe { &mut *server })
                        .push(unsafe { (*lcf).get_server_socket() });

                    let client = async_(move || -> Result<(), ExceptionPtr> {
                        // SAFETY: the inner do_with keeps the socket alive until the client and the server are done.
                        let mut c_socket =
                            unsafe { (*lsi).connect(any_addr(), any_addr(), Transport::TCP) }
                                .get()?;
                        let mut input = c_socket.input(Default::default());
                        let mut output = c_socket.output(8192);
                        let mut more = true;
                        let mut count: usize = 0;
                        while more {
                            let mut htp =
                                HttpConsumer { _concat: false, ..HttpConsumer::default() };

                            Self::write_request(&mut output).get()?;
                            Self::read_reply(&mut input, &mut htp).get()?;
                            println!("{}", String::from_utf8_lossy(&htp._body));
                            more = reader(count, &mut htp);
                            count += 1;
                        }
                        if input.eof() {
                            input.close().get()?;
                        }
                        Ok(())
                    });

                    let server_setup = async_(move || -> Result<(), ExceptionPtr> {
                        let handler =
                            Box::into_raw(Box::new(RunTestHandler::new(server, write_func)));
                        // SAFETY: the outer do_with keeps the server alive until it stops; `srv` is the only reference to it in this thread.
                        let srv = unsafe { &mut *server };
                        srv._routes
                            // SAFETY: the routes own the handler from here on; `handler` is used while
                            // the server lives.
                            .put(OperationType::Get, "/test", unsafe { Box::from_raw(handler) })?;
                        // SAFETY: the server stays in place in the do_with until it stops, so the pin is valid; `srv` is no longer used.
                        let accepts = unsafe { Pin::new_unchecked(&mut *server) }.do_accepts(0);
                        // SAFETY: the routes own the boxed handler from here on and keep it, in place, while the server lives.
                        let _ =
                            when_all((accepts, unsafe { (*handler).wait_for_message() })).get()?;
                        Ok(())
                    });
                    when_all((client, server_setup))
                },
            )
            .discard_result()
            .then_wrapped(move |mut f: Future<()>| {
                f.ignore_ready_future();
                // SAFETY: the outer do_with keeps the server alive until this stop resolves.
                unsafe { (*server).stop() }
            })
        })
    }

    fn run(tests: Vec<(bool, usize)>) -> Future<()> {
        do_with((LoopbackConnectionFactory::new(1), HttpServer::new(&ss("test"))), move |st| {
            let lcf: *mut LoopbackConnectionFactory = &mut st.0;
            let server: *mut HttpServer = &mut st.1;
            // SAFETY: do_with keeps the server in place until it stops: the constructor body
            // runs there.
            unsafe { Pin::new_unchecked(&mut *server) }.start();
            // SAFETY: the do_with()s keep the factory,
            // the server and the socket alive until the server stops.
            do_with(
                LoopbackSocketImpl::new(unsafe { &mut *lcf }, None),
                move |lsi: &mut LoopbackSocketImpl| {
                    let lsi: *mut LoopbackSocketImpl = lsi;
                    // SAFETY: the outer do_with keeps the factory and the server alive until the server stops.
                    HttpServerTester::listeners(unsafe { &mut *server })
                        .push(unsafe { (*lcf).get_server_socket() });

                    let client_tests = tests.clone();
                    let client = async_(move || -> Result<(), ExceptionPtr> {
                        let tests = client_tests;
                        // SAFETY: the inner do_with keeps the socket alive until the client and the server are done.
                        let mut c_socket =
                            unsafe { (*lsi).connect(any_addr(), any_addr(), Transport::TCP) }
                                .get()?;
                        let mut input = c_socket.input(Default::default());
                        let mut output = c_socket.output(8192);
                        let mut more = true;
                        let mut count: usize = 0;
                        while more {
                            let mut htp = HttpConsumer::default();
                            Self::write_request(&mut output).get()?;
                            Self::read_reply(&mut input, &mut htp).get()?;
                            if tests[count].0 {
                                assert_eq!(htp._body.len(), tests[count].1);
                            } else {
                                assert!(input.eof());
                                more = false;
                            }
                            count += 1;
                            if count == tests.len() {
                                more = false;
                            }
                        }
                        if input.eof() {
                            input.close().get()?;
                        }
                        Ok(())
                    });

                    let server_setup = async_(move || -> Result<(), ExceptionPtr> {
                        let handler = Box::into_raw(Box::new(RunHandler::new(server, &tests)));
                        // SAFETY: the outer do_with keeps the server alive until it stops; `srv` is the only reference to it in this thread.
                        let srv = unsafe { &mut *server };
                        srv._routes
                            // SAFETY: the routes own the handler from here on; `handler` is used while
                            // the server lives.
                            .put(OperationType::Get, "/test", unsafe { Box::from_raw(handler) })?;
                        // SAFETY: the server stays in place in the do_with until it stops, so the pin is valid; `srv` is no longer used.
                        let accepts = unsafe { Pin::new_unchecked(&mut *server) }.do_accepts(0);
                        // SAFETY: the routes own the boxed handler from here on and keep it, in place, while the server lives.
                        let _ =
                            when_all((accepts, unsafe { (*handler).wait_for_message() })).get()?;
                        Ok(())
                    });
                    when_all((client, server_setup))
                },
            )
            .discard_result()
            .then_wrapped(move |mut f: Future<()>| {
                f.ignore_ready_future();
                // SAFETY: the outer do_with keeps the server alive until this stop resolves.
                unsafe { (*server).stop() }
            })
        })
    }

    fn make_writer(len: usize, success: bool) -> BodyWriter {
        BodyWriter::new(move |o_stream: OutputStream<u8>| -> Future<()> {
            do_with(
                (o_stream, (len / 10) as u32),
                move |st: &mut (OutputStream<u8>, u32)| -> Future<()> {
                    let stp: *mut (OutputStream<u8>, u32) = st;
                    if st.1 == 0 {
                        if success {
                            return st.0.close();
                        } else {
                            return make_exception_future(ExceptionPtr::new(
                                teseladb::engine::core::future::RuntimeError::new(
                                    "Throwing exception before writing".to_string(),
                                ),
                            ));
                        }
                    }
                    repeat(move || {
                        // SAFETY: do_with keeps the stream and the counter alive.
                        unsafe { (*stp).0.write_str("1234567890") }.then(move |()| {
                            // SAFETY: do_with keeps the counter alive; the write above has resolved, so nothing else references the pair.
                            let remain = unsafe { &mut (*stp).1 };
                            *remain -= 1;
                            make_ready_future(if *remain == 0 {
                                StopIteration::YES
                            } else {
                                StopIteration::NO
                            })
                        })
                    })
                    .then(move |()| {
                        if !success {
                            // SAFETY: do_with keeps the stream alive until the chain resolves.
                            return unsafe { (*stp).0.flush() };
                        }
                        make_ready_future(())
                    })
                    .then(move |()| {
                        if success {
                            // SAFETY: do_with keeps the stream alive until the chain resolves.
                            unsafe { (*stp).0.close() }
                        } else {
                            make_exception_future(ExceptionPtr::new(
                                teseladb::engine::core::future::RuntimeError::new(
                                    "Throwing exception after writing".to_string(),
                                ),
                            ))
                        }
                    })
                },
            )
        })
    }
}

teseladb::engine_test_case!(test_message_with_error_non_empty_body, {
    let tests = vec![(true, 100), (false, 10000)];
    TestClientServer::run(tests)
});

teseladb::engine_test_case!(test_simple_chunked, {
    let tests = vec![(true, 100000), (true, 100)];
    TestClientServer::run(tests)
});

teseladb::engine_test_case!(test_http_client_server_full, {
    let tests = vec![
        (true, 100),
        (true, 10000),
        (true, 100),
        (true, 0),
        (true, 5000),
        (true, 10000),
        (true, 9000),
        (true, 10000),
    ];
    TestClientServer::run(tests)
});

/*
 * return string in the given size
 * The string size takes the quotes into consideration.
 */
fn get_value(size: i32) -> String {
    let mut res = String::new();
    for _ in 0..size - 2 {
        res.push('a');
    }
    res
}

/*
 * A helper object that map to a big json string
 * in the format of:
 * {"valu":"aaa....aa","valu":"aaa....aa","valu":"aaa....aa"...}
 *
 * The object can have an arbitrary size in multiplication of 10000 bytes
 *  */
struct ExtraBigObject {
    value: Box<JsonElement<Sstring>>,
    /// `_elements`: the same element `size / 10000` times.
    elements: usize,
}

impl ExtraBigObject {
    fn new(size: usize) -> Self {
        let mut value = Box::new(JsonElement::<Sstring>::default());
        // size = brackets + ("name" + ":" + get_value()) * n + "," * (n-1)
        // size = 2 + (valu + 4 + get_value) * n - 1
        value.base_mut()._name = "valu".to_string();
        value.set(Sstring::from(get_value(9992).as_str()));
        ExtraBigObject { value, elements: size / 10000 }
    }
}

impl Clone for ExtraBigObject {
    fn clone(&self) -> Self {
        let mut value = Box::new(JsonElement::<Sstring>::default());
        value.base_mut()._name = self.value.base()._name.clone();
        value.set(self.value.get().clone());
        ExtraBigObject { value, elements: self.elements }
    }
}

impl JsonBase for ExtraBigObject {
    fn element_count(&self) -> usize {
        self.elements
    }

    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        if i < self.elements { Some(&*self.value) } else { None }
    }
}

impl Jsonable for ExtraBigObject {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        self.base_to_json()
    }
    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        (*self).base_write(s)
    }
}

/// Writes the object as JSON, as `stream_object` does for each element of a vector of
/// objects.
impl JsonWrite for ExtraBigObject {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        // The object moves into the write, which keeps it until it resolves.
        self.base_write(s)
    }
}

teseladb::engine_test_case!(json_stream, {
    let mut vec: Vec<ExtraBigObject> = Vec::new();
    let num_objects: usize = 1000;
    // each object is 1000001 bytes plus a comma for all but the last and plus
    // two for the [] backets
    let total_size = num_objects * 1000002 + 1;
    for _ in 0..num_objects {
        vec.push(ExtraBigObject::new(1000000));
    }
    TestClientServer::run_test(
        json_body(stream_object(vec)),
        move |_s: usize, h: &mut HttpConsumer| {
            assert_eq!(h._size as usize, total_size);
            false
        },
    )
});

// A body writer that writes a short buffer and closes the stream must not abort.
teseladb::engine_test_case!(dont_abort, {
    TestClientServer::run_test(
        BodyWriter::new(|stream_: OutputStream<u8>| -> Future<()> {
            make_coroutine(async move {
                let mut stream = stream_;
                stream.write_buffer(TemporaryBuffer::new(3)).await?;
                stream.close().await?;
                Ok(())
            })
        }),
        |_s: usize, h: &mut HttpConsumer| {
            assert_eq!(h._size, 3);
            false
        },
    )
});

/// A handler that streams a JSON object. A `BodyWriter` is not copyable, so the handler keeps the function that
/// makes it (`json::stream_object("hello")`) and calls it per request.
struct JsonTestHandler {
    base: HandlerBaseData,
    _write_func: Box<dyn Fn() -> WriteFunc>,
}

impl JsonTestHandler {
    fn new(write_func: Box<dyn Fn() -> WriteFunc>) -> Self {
        JsonTestHandler { base: HandlerBaseData::default(), _write_func: write_func }
    }
}

impl HandlerBase for JsonTestHandler {
    handler_base_data!();

    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        rep.write_body_writer("json", (self._write_func)());
        make_ready_future(rep)
    }
}

fn hello_handler() -> Box<dyn HandlerBase> {
    raw_handler(JsonTestHandler::new(Box::new(|| json_body(stream_object("hello")))))
}

teseladb::engine_test_case!(content_length_limit, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        server.get().set_content_length_limit(11);
        let mut lsi = LoopbackSocketImpl::new(&mut lcf, None);
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());

        let lsip: *mut LoopbackSocketImpl = &mut lsi;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: the socket outlives the client thread, joined below.
            let mut c_socket =
                unsafe { (*lsip).connect(any_addr(), any_addr(), Transport::TCP) }.get()?;
            let mut input = c_socket.input(Default::default());
            let mut output = c_socket.output(8192);

            output.write_sstring(&ss("GET /test HTTP/1.1\r\nHost: test\r\n\r\n")).get()?;
            output.flush().get()?;
            let mut resp = input.read().get()?;
            assert_ne!(find(resp.as_slice(), b"200 OK"), NPOS);

            output
                .write_sstring(&ss(
                    "GET /test HTTP/1.1\r\nHost: test\r\nContent-Length: 11\r\n\r\nxxxxxxxxxxx",
                ))
                .get()?;
            output.flush().get()?;
            resp = input.read().get()?;
            assert_ne!(find(resp.as_slice(), b"200 OK"), NPOS);

            output.write_sstring(&ss("GET /test HTTP/1.1\r\nHost: test\r\nContent-Length: 17\r\n\r\nxxxxxxxxxxxxxxxx")).get()?;
            output.flush().get()?;
            resp = input.read().get()?;
            assert_eq!(find(resp.as_slice(), b"200 OK"), NPOS);
            assert_ne!(find(resp.as_slice(), b"413 Payload Too Large"), NPOS);

            input.close().get()?;
            output.close().get()?;
            Ok(())
        });

        server.get()._routes.put(OperationType::Get, "/test", hello_handler())?;
        server.pin().do_accepts(0).get()?;

        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_unexpected_reply_status, {
    async_(|| -> Result<(), ExceptionPtr> {
        #[derive(Default)]
        struct Handl {
            base: HandlerBaseData,
        }
        impl HandlerBase for Handl {
            handler_base_data!();
            fn handle(
                &mut self,
                _path: &Sstring,
                _req: Box<Request>,
                _rep: Box<Reply>,
            ) -> Future<Box<Reply>> {
                println!("Returning exception from server");
                make_exception_future(ExceptionPtr::new(server_error_exception("error")))
            }
        }

        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = default_loopback_client(lcfp);
            {
                let req = get_request("/test");
                let r = cln
                    .as_mut()
                    .make_request(
                        req,
                        reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                            // Must not be reached.
                            panic!("should throw before handling response")
                        }),
                        Some(StatusType::OK),
                    )
                    .get();
                assert!(r.is_err());
                let Err(e) = r else { unreachable!() };
                assert_eq!(http_exception_kind(&e), Some(ExceptionKind::UnexpectedStatus));
            }
            {
                let req = get_request("/test");
                let status: Rc<Cell<Option<StatusType>>> = Rc::new(Cell::new(None));
                let st = status.clone();
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |rep: &Reply, _in: InputStream<u8>| {
                            st.set(Some(rep._status));
                            make_ready_future(())
                        }),
                        None,
                    )
                    .get()?;
                assert!(status.get().is_some());
                assert_eq!(status.get(), Some(StatusType::INTERNAL_SERVER_ERROR));
            }

            cln.as_mut().close().get()?;
            Ok(())
        });

        server.get()._routes.put(OperationType::Get, "/test", raw_handler(Handl::default()))?;
        server.pin().do_accepts(0).get()?;
        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
});

fn read_simple_http_request(in_: &mut InputStream<u8>) -> Result<(), ExceptionPtr> {
    let mut req = Sstring::new();
    loop {
        let r = in_.read().get()?;
        req.append_bytes(r.as_slice());
        if req.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(())
}

/// Serves one accepted connection with `body` in a new engine thread.
fn serve_connection<F>(ar: AcceptResult, body: F) -> Future<()>
where
    F: FnOnce(&mut ConnectedSocket) -> Result<(), ExceptionPtr> + 'static,
{
    let mut sk = ar.connection;
    async_(move || body(&mut sk))
}

fn write_and_close(sk: &mut ConnectedSocket, data: &str) -> Result<(), ExceptionPtr> {
    let mut in_ = sk.input(Default::default());
    read_simple_http_request(&mut in_)?;
    let mut out = sk.output(8192);
    out.write_str(data).get()?;
    out.flush().get()?;
    out.close().get()?;
    Ok(())
}

teseladb::engine_test_case!(test_client_response_eof, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let server = ss_.accept().then(|ar: AcceptResult| {
            serve_connection(ar, |sk| {
                write_and_close(sk, "HTT") // write incomplete response
            })
        });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = default_loopback_client(lcfp);
            let req = get_request("/test");
            let r = cln
                .as_mut()
                .make_request(
                    req,
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    Some(StatusType::OK),
                )
                .get();
            assert!(r.is_err());
            let Err(e) = r else { unreachable!() };
            assert_eq!(system_error_code(&e), Some(ECONNABORTED));

            cln.as_mut().close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_retry_nested, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let ssp: *mut _ = &mut ss_;
        let server = ss_
            .accept()
            .then(|ar: AcceptResult| {
                serve_connection(ar, |sk| {
                    // now write complete response
                    write_and_close(sk, "HTTP/1.1 200 OK\r\nHost: localhost\r\n\r\n")
                })
            })
            .then(move |()| {
                // SAFETY: the server socket outlives the server chain, joined below.
                unsafe { (*ssp).accept() }.then(|ar: AcceptResult| {
                    serve_connection(ar, |sk| {
                        // now write complete response
                        write_and_close(sk, "HTTP/1.1 200 OK\r\nHost: localhost\r\n\r\n")
                    })
                })
            });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client_ex = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 2, RetryRequests::YES);
            let req = get_request("/test");
            let count = Rc::new(Cell::new(0usize));
            let c = count.clone();
            let r = cln
                .as_mut()
                .make_request(
                    req,
                    reply_handler(move |_rep: &Reply, _in: InputStream<u8>| {
                        c.set(c.get() + 1);
                        // a system error (EPIPE), nested in a runtime error,
                        // nested in a system error (ENOBUFS)
                        let epipe = ExceptionPtr::new(SystemError::from_errno(EPIPE));
                        let some = ExceptionPtr::with_nested(
                            RuntimeError("Some exception".to_string()),
                            epipe,
                        );
                        let enobufs =
                            ExceptionPtr::with_nested(SystemError::from_errno(ENOBUFS), some);
                        make_exception_future(enobufs)
                    }),
                    Some(StatusType::OK),
                )
                .get();
            assert!(r.is_err());
            let Err(e) = r else { unreachable!() };
            assert_eq!(system_error_code(&e), Some(ENOBUFS));

            cln.as_mut().close().get()?;
            assert_eq!(count.get(), 2);
            Ok(())
        });

        when_all((client_ex, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_response_parse_error, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let server = ss_.accept().then(|ar: AcceptResult| {
            serve_connection(ar, |sk| {
                write_and_close(sk, "HTTTT") // write invalid line
            })
        });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = default_loopback_client(lcfp);
            let req = get_request("/test");
            let r = cln
                .as_mut()
                .make_request(
                    req,
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    Some(StatusType::OK),
                )
                .get();
            assert!(r.is_err());
            let Err(e) = r else { unreachable!() };
            assert!(e.downcast_ref::<BaseException>().is_some());
            let Some(ex) = e.downcast_ref::<BaseException>() else { unreachable!() };
            assert_eq!(*ex.kind(), ExceptionKind::ResponseParsing);
            assert!(ex
                    .what()
                    .contains("Invalid http server response. Reason: Parsing error at offset 3: encountered \"TT\"."));

            cln.as_mut().close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_abort_new_conn, {
    struct DelayedFactory;
    impl ConnectionFactory for DelayedFactory {
        fn make(&mut self, as_: *mut AbortSource) -> Future<ConnectedSocket> {
            assert!(!as_.is_null());
            // SAFETY: the request's abort source outlives the connection attempt.
            sleep_abortable_with::<SteadyClockType>(Duration::from_secs(1), unsafe {
                Pin::new_unchecked(&mut *as_)
            })
            .then(|()| {
                make_exception_future::<ConnectedSocket>(ExceptionPtr::new(
                    teseladb::engine::core::future::RuntimeError::new(
                        "Shouldn't happen".to_string(),
                    ),
                ))
            })
        }
    }

    {
        async_(|| -> Result<(), ExceptionPtr> {
            let mut cln = Box::pin(Client::with_factory(
                Box::new(DelayedFactory),
                Client::DEFAULT_MAX_CONNECTIONS,
                RetryRequests::NO,
                Client::DEFAULT_MAX_BYTES_TO_DRAIN,
            ));
            let mut as_ = Box::pin(AbortSource::new());
            // SAFETY: the source is pinned in its box and outlives the request.
            let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
            // SAFETY: the abort source is pinned and outlives the request.
            let f = unsafe {
                cln.as_mut().make_request_abortable(
                    get_request("/test"),
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    asp,
                    Some(StatusType::OK),
                )
            };

            as_.as_mut().request_abort();
            assert!(f.get().is_err_and(|e| e.is::<AbortRequestedException>()));
            cln.as_mut().close().get()?;
            Ok(())
        })
    }
});

teseladb::engine_test_case!(test_client_abort_cached_conn, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let mut server_paused = Promise::<()>::new();
        let mut server_resume = Promise::<()>::new();
        let spp: *mut Promise<()> = &mut server_paused;
        let srp: *mut Promise<()> = &mut server_resume;
        let server = ss_.accept().then(move |ar: AcceptResult| {
            serve_connection(ar, move |sk| {
                let mut in_ = sk.input(Default::default());
                read_simple_http_request(&mut in_)?;
                // SAFETY: both promises outlive the server and the
                // client, joined below.
                unsafe { (*spp).set_value(()) };
                // SAFETY: `server_resume` outlives the server and the client, joined below.
                unsafe { (*srp).get_future() }.get()?;
                let mut out = sk.output(8192);
                out.close().get()?;
                Ok(())
            })
        });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 1 /* max connections */, RetryRequests::NO);
            // this request gets handled by server and ...
            let f1 = cln.as_mut().make_request(
                get_request("/test"),
                reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                    make_exception_future(ExceptionPtr::new(
                        teseladb::engine::core::future::RuntimeError::new(
                            "Shouldn't happen".to_string(),
                        ),
                    ))
                }),
                Some(StatusType::OK),
            );
            // SAFETY: `server_paused` outlives the server and the client, joined below.
            unsafe { (*spp).get_future() }.get()?;
            // ... this should hang waiting for cached connection
            let mut as_ = Box::pin(AbortSource::new());
            // SAFETY: the source is pinned in its box and outlives the request.
            let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
            // SAFETY: the abort source is pinned and outlives the request.
            let f2 = unsafe {
                cln.as_mut().make_request_abortable(
                    get_request("/test"),
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    asp,
                    Some(StatusType::OK),
                )
            };

            as_.as_mut().request_abort();
            assert!(f2.get().is_err_and(|e| e.is::<AbortRequestedException>()));
            // SAFETY: `server_resume` outlives the server and the client, joined below.
            unsafe { (*srp).set_value(()) };
            cln.as_mut().close().get()?;
            let _ = f1.get();
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_abort_send_request, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let server = ss_.accept().then(|ar: AcceptResult| {
            serve_connection(ar, |sk| {
                let mut in_ = sk.input(Default::default());
                read_simple_http_request(&mut in_)?;
                let mut out = sk.output(8192);
                out.close().get()?;
                Ok(())
            })
        });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 1 /* max connections */, RetryRequests::NO);
            let mut as_ = Box::pin(AbortSource::new());
            // SAFETY: the source is pinned in its box and outlives the request.
            let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
            let mut req = get_request("/test");
            let mut client_paused = Promise::<()>::new();
            let mut client_resume = Promise::<()>::new();
            let cpp: *mut Promise<()> = &mut client_paused;
            let crp: *mut Promise<()> = &mut client_resume;
            // SAFETY: both promises outlive the request.
            req.write_body_writer(
                "txt",
                BodyWriter::new(move |out: OutputStream<u8>| -> Future<()> {
                    let mut out = out;
                    async_(move || -> Result<(), ExceptionPtr> {
                        let op: *mut OutputStream<u8> = &mut out;
                        // SAFETY: `out` lives in this thread's frame, after the guard.
                        let mut closer = StreamCloser(op);
                        let _cl = DeferredClose::new(&mut closer);
                        // SAFETY: `client_paused` outlives the request and its body writer.
                        unsafe { (*cpp).set_value(()) };
                        // SAFETY: `client_resume` outlives the request and its body writer.
                        unsafe { (*crp).get_future() }.get()?;
                        // SAFETY: `op` points at `out`, which lives in this thread's frame until the thread ends.
                        unsafe { (*op).write_str("foo") }.get()?;
                        // SAFETY: `op` points at `out`, which lives in this thread's frame until the thread ends.
                        unsafe { (*op).flush() }.get()?;
                        Ok(())
                    })
                }),
            );
            // SAFETY: the abort source is pinned and outlives the request.
            let f = unsafe {
                cln.as_mut().make_request_abortable(
                    req,
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    asp,
                    Some(StatusType::OK),
                )
            };
            // SAFETY: `client_paused` outlives the request and its body writer.
            unsafe { (*cpp).get_future() }.get()?;
            as_.as_mut().request_abort();
            // SAFETY: `client_resume` outlives the request and its body writer.
            unsafe { (*crp).set_value(()) };
            assert!(f.get().is_err_and(|e| e.is::<AbortRequestedException>()));
            cln.as_mut().close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_abort_recv_response, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let mut server_paused = Promise::<()>::new();
        let mut server_resume = Promise::<()>::new();
        let spp: *mut Promise<()> = &mut server_paused;
        let srp: *mut Promise<()> = &mut server_resume;
        // SAFETY: both promises outlive the server and the
        // client, joined below.
        let server = ss_.accept().then(move |ar: AcceptResult| {
            serve_connection(ar, move |sk| {
                let mut in_ = sk.input(Default::default());
                read_simple_http_request(&mut in_)?;
                // SAFETY: `server_paused` outlives the server and the client, joined below.
                unsafe { (*spp).set_value(()) };
                // SAFETY: `server_resume` outlives the server and the client, joined below.
                unsafe { (*srp).get_future() }.get()?;
                let mut out = sk.output(8192);
                out.close().get()?;
                Ok(())
            })
        });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 1 /* max connections */, RetryRequests::NO);
            let mut as_ = Box::pin(AbortSource::new());
            // SAFETY: the source is pinned in its box and outlives the request.
            let asp: *mut AbortSource = unsafe { as_.as_mut().get_unchecked_mut() };
            // SAFETY: the abort source is pinned and outlives the request.
            let f = unsafe {
                cln.as_mut().make_request_abortable(
                    get_request("/test"),
                    reply_handler(|_rep: &Reply, _in: InputStream<u8>| {
                        make_exception_future(ExceptionPtr::new(
                            teseladb::engine::core::future::RuntimeError::new(
                                "Shouldn't happen".to_string(),
                            ),
                        ))
                    }),
                    asp,
                    Some(StatusType::OK),
                )
            };
            // SAFETY: `server_paused` outlives the server and the client, joined below.
            unsafe { (*spp).get_future() }.get()?;
            as_.as_mut().request_abort();
            assert!(f.get().is_err_and(|e| e.is::<AbortRequestedException>()));
            // SAFETY: `server_resume` outlives the server and the client, joined below.
            unsafe { (*srp).set_value(()) };
            cln.as_mut().close().get()?;
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_retry_request, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut ss_ = lcf.get_server_socket();
        let ssp: *mut _ = &mut ss_;
        let server = ss_
            .accept()
            .then(|ar: AcceptResult| {
                serve_connection(ar, |sk| {
                    write_and_close(sk, "HTTTT") // write incomplete response
                })
            })
            .then(move |()| {
                // SAFETY: the server socket outlives the server chain, joined below.
                unsafe { (*ssp).accept() }.then(|ar: AcceptResult| {
                    serve_connection(ar, |sk| {
                        // now write complete response
                        write_and_close(sk, "HTTP/1.1 200 OK\r\nHost: localhost\r\n\r\n")
                    })
                })
            });

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 2, RetryRequests::YES);
            let req = get_request("/test");
            let got_response = Rc::new(Cell::new(false));
            let g = got_response.clone();
            cln.as_mut()
                .make_request(
                    req,
                    reply_handler(move |_rep: &Reply, _in: InputStream<u8>| {
                        g.set(true);
                        make_ready_future(())
                    }),
                    Some(StatusType::OK),
                )
                .get()?;
            cln.as_mut().close().get()?;
            assert!(got_response.get());
            Ok(())
        });

        when_all((client, server)).discard_result().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_100_continue, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        server.get().set_content_length_limit(11);
        let mut lsi = LoopbackSocketImpl::new(&mut lcf, None);
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());
        let lsip: *mut LoopbackSocketImpl = &mut lsi;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: the socket outlives the client thread, joined below.
            let mut c_socket =
                unsafe { (*lsip).connect(any_addr(), any_addr(), Transport::TCP) }.get()?;
            let mut input = c_socket.input(Default::default());
            let mut output = c_socket.output(8192);

            for version in [ss("1.0"), ss("1.1")] {
                for content in [ss(""), ss("xxxxxxxxxxx")] {
                    for expect in
                        [ss(""), ss("Expect: 100-continue\r\n"), ss("Expect: 100-cOnTInUE\r\n")]
                    {
                        let need_cont = version == "1.1" && expect.length() != 0;
                        let mut body_sent = false;
                        let content_len = if content.empty() {
                            ss("")
                        } else {
                            ss(&format!("Content-Length: {}\r\n", to_sstring(&content.length())))
                        };
                        let req = ss(&format!(
                            "GET /test HTTP/{}\r\nHost: test\r\nConnection: Keep-Alive\r\n{}{}\r\n",
                            version, content_len, expect
                        ));
                        let mut resp = Sstring::new();

                        output.write_sstring(&req).get()?;
                        output.flush().get()?;

                        if !need_cont {
                            output.write_sstring(&content).get()?;
                            output.flush().get()?;
                            body_sent = true;
                        }

                        loop {
                            let r = input.read().get()?;
                            assert!(!r.empty());
                            resp.append_bytes(r.as_slice());

                            if need_cont
                                && !body_sent
                                && find(resp.as_bytes(), b"100 Continue") != NPOS
                            {
                                output.write_sstring(&content).get()?;
                                output.flush().get()?;
                                body_sent = true;
                            }

                            if resp.ends_with(b"\"hello\"\r\n0\r\n\r\n") {
                                break;
                            }
                        }

                        if need_cont {
                            assert!(body_sent);
                        }
                        assert_ne!(find(resp.as_bytes(), b"200 OK"), NPOS);
                    }
                }
            }
            output
                    .write_sstring(&ss("GET /test HTTP/1.1\r\nHost: test\r\nContent-Length: 17\r\nExpect: 100-continue\r\n\r\n"))
                    .get()?;
            output.flush().get()?;
            let resp = input.read().get()?;
            assert_eq!(find(resp.as_slice(), b"100 Continue"), NPOS);
            assert_ne!(find(resp.as_slice(), b"413 Payload Too Large"), NPOS);

            input.close().get()?;
            output.close().get()?;
            Ok(())
        });

        server.get()._routes.put(OperationType::Get, "/test", hello_handler())?;
        server.pin().do_accepts(0).get()?;

        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_unparsable_request, {
    // Test if a message that cannot be parsed as a http request is being replied with a 400 Bad Request response
    {
        async_(|| -> Result<(), ExceptionPtr> {
            let mut lcf = LoopbackConnectionFactory::new(1);
            let mut server = TestServer::new(&ss("test"));
            let mut lsi = LoopbackSocketImpl::new(&mut lcf, None);
            HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());
            let lsip: *mut LoopbackSocketImpl = &mut lsi;
            let client = async_(move || -> Result<(), ExceptionPtr> {
                // SAFETY: the socket outlives the client thread, joined below.
                let mut c_socket =
                    unsafe { (*lsip).connect(any_addr(), any_addr(), Transport::TCP) }.get()?;
                let mut input = c_socket.input(Default::default());
                let mut output = c_socket.output(8192);

                output
                    .write_sstring(&ss("GET /test HTTP/1.1\r\nhello\r\nContent-Length: 17\r\nExpect: 100-continue\r\n\r\n"))
                    .get()?;
                output.flush().get()?;
                let resp = input.read().get()?;
                assert_ne!(find(resp.as_slice(), b"400 Bad Request"), NPOS);
                assert_ne!(find(resp.as_slice(), b"Can't parse the request"), NPOS);

                input.close().get()?;
                output.close().get()?;
                Ok(())
            });

            server.get()._routes.put(OperationType::Get, "/test", hello_handler())?;
            server.pin().do_accepts(0).get()?;

            client.get()?;
            server.get().stop().get()?;
            Ok(())
        })
    }
});

/// A handler that replies with the request body, chunked or not.
struct EchoHandler {
    base: HandlerBaseData,
    chunked_reply: bool,
}

impl EchoHandler {
    fn new(chunked_reply_: bool) -> Self {
        EchoHandler { base: HandlerBaseData::default(), chunked_reply: chunked_reply_ }
    }

    fn do_handle(
        &self,
        req: &Request,
        mut rep: Box<Reply>,
        content: &mut Sstring,
    ) -> Future<Box<Reply>> {
        for (k, v) in &req.chunk_extensions {
            content.append_bytes(k.as_bytes());
            if !v.is_empty() {
                content.append_bytes(b"=");
                content.append_bytes(v.as_bytes());
            }
        }
        for (k, v) in &req.trailing_headers {
            content.append_bytes(k.as_bytes());
            if !v.is_empty() {
                content.append_bytes(b": ");
                content.append_bytes(v.as_bytes());
            }
        }
        if !self.chunked_reply {
            rep.write_body("txt", content.clone());
        } else {
            let c = content.clone();
            rep.write_body_with("txt", move |out: &mut OutputStream<u8>| -> Future<()> {
                let outp: *mut OutputStream<u8> = out;
                make_coroutine(async move {
                    // SAFETY: the reply keeps the stream alive until the writer resolves.
                    unsafe { (*outp).write_sstring(&c) }.await?;
                    Ok(())
                })
            });
        }
        make_ready_future(rep)
    }
}

/*
 * A request handler that responds with the same body that was used in the request using the requests content_stream
 *  */
struct EchoStreamHandler {
    echo: EchoHandler,
}

impl EchoStreamHandler {
    fn new(chunked_reply: bool) -> Self {
        EchoStreamHandler { echo: EchoHandler::new(chunked_reply) }
    }
}

impl HandlerBase for EchoStreamHandler {
    fn base(&self) -> &HandlerBaseData {
        &self.echo.base
    }
    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.echo.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        let this: *mut EchoStreamHandler = self;
        do_with(
            (req, Some(rep), Sstring::new()),
            move |st: &mut (Box<Request>, Option<Box<Reply>>, Sstring)| {
                let stp: *mut (Box<Request>, Option<Box<Reply>>, Sstring) = st;
                // SAFETY: do_with keeps the request, the reply and the content
                // alive; the request's content stream lives as long as the request; the routes
                // keep the handler alive.
                do_until(
                    move || unsafe { (*(*stp).0.content_stream).eof() },
                    move || {
                        // SAFETY: do_with keeps the request alive, and its content stream lives as long as the request.
                        unsafe { (*(*stp).0.content_stream).read() }.then(
                            move |tmp: TemporaryBuffer<u8>| {
                                // SAFETY: do_with keeps the content string alive; the read has resolved, so nothing else references it.
                                unsafe {
                                    (*stp).2.append_bytes(to_sstring_buffer(&tmp).as_bytes())
                                };
                            },
                        )
                    },
                )
                .then(move |()| {
                    // SAFETY: do_with keeps the request, the reply and the content alive until this chain resolves.
                    let st = unsafe { &mut *stp };
                    let rep = st.1.take().unwrap_or_default();
                    // SAFETY: the routes keep the handler alive while the request is served.
                    unsafe { (*this).echo.do_handle(&st.0, rep, &mut st.2) }
                })
            },
        )
    }
}

/*
 * Same handler as above, but without using streams
 *  */
struct EchoStringHandler {
    echo: EchoHandler,
}

impl EchoStringHandler {
    fn new(chunked_reply: bool) -> Self {
        EchoStringHandler { echo: EchoHandler::new(chunked_reply) }
    }
}

impl HandlerBase for EchoStringHandler {
    fn base(&self) -> &HandlerBaseData {
        &self.echo.base
    }
    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.echo.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        mut req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        let mut content = std::mem::take(&mut req.content);
        let f = self.echo.do_handle(&req, rep, &mut content);
        req.content = content;
        f
    }
}

/*
 * Checks if the server responds to the request equivalent to the concatenation of all req_parts with a reply containing
 * the resp_parts strings, assuming that the content streaming is set to stream and the /test route is handled by handl
 * */
fn check_http_reply(
    req_parts: Vec<&'static [u8]>,
    resp_parts: Vec<&'static [u8]>,
    stream: bool,
    handl: Box<dyn HandlerBase>,
) -> Future<()> {
    async_(move || -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        server.get().set_content_streaming(stream);
        let mut lsi = LoopbackSocketImpl::new(&mut lcf, None);
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());
        let lsip: *mut LoopbackSocketImpl = &mut lsi;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            // SAFETY: the socket outlives the client thread, joined below.
            let mut c_socket =
                unsafe { (*lsip).connect(any_addr(), any_addr(), Transport::TCP) }.get()?;
            let mut input = c_socket.input(Default::default());
            let mut output = c_socket.output(8192);

            for str in &req_parts {
                output.write(str).get()?;
                output.flush().get()?;
            }
            let resp = input.read().get()?;
            for str in &resp_parts {
                assert_ne!(find(resp.as_slice(), str), NPOS);
            }

            input.close().get()?;
            output.close().get()?;
            Ok(())
        });

        server.get()._routes.put(OperationType::Get, "/test", handl)?;
        server.pin().do_accepts(0).get()?;

        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
}

fn test_basic_content(streamed: bool, chunked_reply: bool) -> Future<()> {
    async_(move || -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        if streamed {
            server.get().set_content_streaming(true);
        }
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());
        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = default_loopback_client(lcfp);

            {
                println!("Simple request test");
                let req = get_request("/test");
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            assert_eq!(resp.get_header("Server"), "TeselaDB httpd");
                            if chunked_reply {
                                // need to drop empty chunk
                                let mut in_ = in_;
                                return async_(move || {
                                    // SAFETY: `in_` lives in the thread's frame while the skip is waited for.
                                    unsafe { util::skip_entire_stream(&mut in_) }.get()
                                });
                            }
                            make_ready_future(())
                        }),
                        Some(StatusType::OK),
                    )
                    .get()?;
                // in fact, this case is to make sure that _next_ cases won't collect
                // garbage from the client connection
            }

            {
                println!("Request with body test");
                let mut req = get_request("/test");
                req.write_body("txt", ss("12345 78901\t34521345"));
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            assert_eq!(resp._status, StatusType::OK);
                            if !chunked_reply {
                                assert_eq!(resp.content_length, 20);
                            }
                            let mut in_ = in_;
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, ss("12345 78901\t34521345"));
                                Ok(())
                            })
                        }),
                        None,
                    )
                    .get()?;
            }

            {
                println!("Request with content-length body");
                let mut req = get_request("/test");
                req.write_body_len_with("txt", 12, |out: &mut OutputStream<u8>| -> Future<()> {
                    let outp: *mut OutputStream<u8> = out;
                    make_coroutine(async move {
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_sstring(&ss("1234567890")) }.await?;
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_sstring(&ss("AB")) }.await?;
                        Ok(())
                    })
                });
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            if !chunked_reply {
                                assert_eq!(resp.content_length, 12);
                            }
                            let mut in_ = in_;
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, ss("1234567890AB"));
                                Ok(())
                            })
                        }),
                        Some(StatusType::OK),
                    )
                    .get()?;
            }

            {
                const SIZE: usize = 128 * 1024;
                let size = SIZE;
                println!("Request with {}-kbytes content-length body", size >> 10);
                let mut jumbo = TemporaryBuffer::<u8>::new(size);
                let mut jumbo_copy = TemporaryBuffer::<u8>::new(size);
                // SAFETY: both buffers hold `size` bytes.
                let (jw, jcw) = unsafe {
                    (
                        std::slice::from_raw_parts_mut(jumbo.get_write(), size),
                        std::slice::from_raw_parts_mut(jumbo_copy.get_write(), size),
                    )
                };
                for i in 0..size {
                    jw[i] = b'a' + (i % (b'z' - b'a') as usize) as u8;
                    jcw[i] = jw[i];
                }
                let mut req = get_request("/test");
                req.write_body_len_with(
                    "txt",
                    size,
                    move |out: &mut OutputStream<u8>| -> Future<()> {
                        let outp: *mut OutputStream<u8> = out;
                        make_coroutine(async move {
                            // SAFETY: the request keeps the stream alive until the writer resolves.
                            unsafe { (*outp).write(jumbo.as_slice()) }.await?;
                            Ok(())
                        })
                    },
                );
                let mut jumbo_copy = Some(jumbo_copy);
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            if !chunked_reply {
                                assert_eq!(resp.content_length, size);
                            }
                            let mut in_ = in_;
                            let jumbo_copy = jumbo_copy.take().unwrap_or_default();
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, to_sstring_buffer(&jumbo_copy));
                                Ok(())
                            })
                        }),
                        Some(StatusType::OK),
                    )
                    .get()?;
            }

            {
                println!("Request whose content-length body is written via a temporary_buffer");
                let mut req = get_request("/test");
                req.write_body_len_with("txt", 5, |out: &mut OutputStream<u8>| -> Future<()> {
                    let outp: *mut OutputStream<u8> = out;
                    make_coroutine(async move {
                        let msg: &[u8] = b"hello";
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_buffer(TemporaryBuffer::copy_from(&msg[..5])) }
                            .await?;
                        Ok(())
                    })
                });
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            assert_eq!(resp._status, StatusType::OK);
                            if !chunked_reply {
                                assert_eq!(resp.content_length, 5);
                            }
                            let mut in_ = in_;
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, ss("hello"));
                                Ok(())
                            })
                        }),
                        None,
                    )
                    .get()?;
            }

            {
                println!("Request with chunked body");
                let mut req = get_request("/test");
                req.write_body_with("txt", |out: &mut OutputStream<u8>| -> Future<()> {
                    let outp: *mut OutputStream<u8> = out;
                    make_coroutine(async move {
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_sstring(&ss("req")) }.await?;
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_sstring(&ss("1234\r\n7890")) }.await?;
                        Ok(())
                    })
                });
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            assert_eq!(resp._status, StatusType::OK);
                            if !chunked_reply {
                                assert_eq!(resp.content_length, 13);
                            }
                            let mut in_ = in_;
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, ss("req1234\r\n7890"));
                                Ok(())
                            })
                        }),
                        None,
                    )
                    .get()?;
            }

            {
                println!("Request with expect-continue");
                let mut req = get_request("/test");
                req.write_body("txt", ss("foobar"));
                req.set_expects_continue();
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                            assert_eq!(resp._status, StatusType::OK);
                            if !chunked_reply {
                                assert_eq!(resp.content_length, 6);
                            }
                            let mut in_ = in_;
                            async_(move || -> Result<(), ExceptionPtr> {
                                // SAFETY: `in_` lives in the thread's frame while the read is waited for.
                                let body = unsafe { util::read_entire_stream_contiguous(&mut in_) }
                                    .get()?;
                                assert_eq!(body, ss("foobar"));
                                Ok(())
                            })
                        }),
                        None,
                    )
                    .get()?;
            }

            {
                println!("Request with incomplete content-length body");
                let mut req = get_request("/test");
                req.write_body_len_with("txt", 12, |out: &mut OutputStream<u8>| -> Future<()> {
                    let outp: *mut OutputStream<u8> = out;
                    make_coroutine(async move {
                        // SAFETY: the request keeps the stream alive until the writer resolves.
                        unsafe { (*outp).write_sstring(&ss("1234567890A")) }.await?;
                        Ok(())
                    })
                });
                let r = cln
                    .as_mut()
                    .make_request(
                        req,
                        reply_handler(|_resp: &Reply, _in: InputStream<u8>| {
                            // Must not be reached.
                            panic!("should throw before handling response")
                        }),
                        None,
                    )
                    .get();
                assert!(r.is_err_and(|e| is_runtime_error(&e)));
            }

            {
                let callback_completed = Rc::new(Cell::new(false));
                println!("Request with too large content-length body");
                let mut req = get_request("/test");
                let cc = callback_completed.clone();
                req.write_body_len_writer(
                    "txt",
                    12,
                    BodyWriter::new(move |out: OutputStream<u8>| -> Future<()> {
                        let mut out = out;
                        let cc = cc.clone();
                        async_(move || -> Result<(), ExceptionPtr> {
                            out.write_sstring(&ss("1234567890ABC")).get()?;
                            out.flush().get()?;
                            out.close().get()?;
                            cc.set(true);
                            Ok(())
                        })
                    }),
                );
                assert!(!callback_completed.get()); // should throw early
                let r = cln
                    .as_mut()
                    .make_request(
                        req,
                        reply_handler(|_resp: &Reply, _in: InputStream<u8>| {
                            // Must not be reached.
                            panic!("should throw before handling response")
                        }),
                        None,
                    )
                    .get();
                assert!(r.is_err_and(|e| is_runtime_error(&e)));
            }

            cln.as_mut().close().get()?;
            Ok(())
        });

        let handler: Box<dyn HandlerBase> = if streamed {
            raw_handler(EchoStreamHandler::new(chunked_reply))
        } else {
            raw_handler(EchoStringHandler::new(chunked_reply))
        };
        server.get()._routes.put(OperationType::Get, "/test", handler)?;
        server.pin().do_accepts(0).get()?;

        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
}

teseladb::engine_test_case!(test_string_content, { test_basic_content(false, false) });

teseladb::engine_test_case!(test_string_content_chunked, { test_basic_content(false, true) });

teseladb::engine_test_case!(test_stream_content, { test_basic_content(true, false) });

teseladb::engine_test_case!(test_stream_content_chunked, { test_basic_content(true, true) });

teseladb::engine_test_case!(test_not_implemented_encoding, {
    check_http_reply(
            vec![
                b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
                b"a\r\n1234567890\r\n",
                b"a\r\n1234521345\r\n",
                b"0\r\n\r\n",
            ],
            vec![
                b"501 Not Implemented",
                b"Encodings other than \"chunked\" are not implemented (received encoding: \"gzip, chunked\")",
            ],
            false,
            raw_handler(EchoStringHandler::new(false)),
        )
});

teseladb::engine_test_case!(test_full_chunk_format, {
    check_http_reply(
            vec![
                b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
                b"a;abc-def;hello=world;aaaa\r\n1234567890\r\n",
                b"a;a0-!#$%&'*+.^_`|~=\"quoted string obstext\x80\x81\xff quoted_pair: \\a\"\r\n1234521345\r\n",
                b"0\r\na:b\r\n~|`_^.+*'&%$#!-0a:  ~!@#$%^&*()_+\x80\x81\xff\r\n  obs fold  \r\n\r\n",
            ],
            vec![
                b"12345678901234521345",
                b"abc-def",
                b"hello=world",
                b"aaaa",
                b"a0-!#$%&'*+.^_`|~=quoted string obstext\x80\x81\xff quoted_pair: a",
                b"a: b",
                b"~|`_^.+*'&%$#!-0a: ~!@#$%^&*()_+\x80\x81\xff obs fold",
            ],
            false,
            raw_handler(EchoStringHandler::new(false)),
        )
});

teseladb::engine_test_case!(test_chunk_extension_parser_fail, {
    check_http_reply(
        vec![
            b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"7; \r\nnoparse\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"400 Bad Request", b"Can't parse chunk size and extensions"],
        false,
        raw_handler(EchoStringHandler::new(false)),
    )
});

teseladb::engine_test_case!(test_trailer_part_parser_fail, {
    check_http_reply(
        vec![
            b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"8\r\nparsable\r\n",
            b"0\r\ngood:header\r\nbad=header\r\n\r\n",
        ],
        vec![b"400 Bad Request", b"Can't parse chunked request trailer"],
        false,
        raw_handler(EchoStringHandler::new(false)),
    )
});

teseladb::engine_test_case!(test_too_long_chunk, {
    check_http_reply(
        vec![
            b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"a\r\n1234567890\r\n",
            b"a\r\n1234521345X\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"400 Bad Request", b"The actual chunk length exceeds the specified length"],
        true,
        raw_handler(EchoStreamHandler::new(false)),
    )
});

teseladb::engine_test_case!(test_bad_chunk_length, {
    check_http_reply(
        vec![
            b"GET /test HTTP/1.1\r\nHost: test\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"a\r\n1234567890\r\n",
            b"aX\r\n1234521345\r\n",
            b"0\r\n\r\n",
        ],
        vec![b"400 Bad Request", b"Can't parse chunk size and extensions"],
        true,
        raw_handler(EchoStreamHandler::new(false)),
    )
});

teseladb::engine_test_case!(case_insensitive_header, {
    let mut req = Box::new(Request::default());
    req._headers.insert(CaseInsensitiveKey(ss("conTEnt-LengtH")), ss("17"));
    assert_eq!(req.get_header("content-length"), "17");
    assert_eq!(req.get_header("Content-Length"), "17");
    assert_eq!(req.get_header("cOnTeNT-lEnGTh"), "17");
    make_ready_future(())
});

/// Parses all of `r` as one NUL-terminated buffer.
fn parse_all(parser: &mut HttpResponseParser, r: &[u8]) {
    let mut buf: Vec<u8> = r.to_vec();
    buf.push(0); // the input counts its terminating NUL
    // The responses are parsed from buffers that must outlive the parser's result: the copy is
    // leaked so that it outlives the parsed response the caller takes afterwards.
    let buf: &'static mut [u8] = buf.leak();
    let p = buf.as_mut_ptr();
    // SAFETY: `p .. p + len` is the leaked buffer.
    let pe = unsafe { p.add(buf.len()) };
    // SAFETY: `[p, pe)` is the leaked buffer, live for the rest of the program and reached
    // only through these pointers from here on.
    unsafe { parser.parse(p, pe, pe) };
}

teseladb::engine_test_case!(broken_reply, {
    let mut parser = HttpResponseParser::default();
    parser.init();
    let r101: &[u8] = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit.";

    parse_all(&mut parser, r101);
    assert!(parser.failed());
    assert!(parser.error_message().starts_with(
        b"Parsing error at offset 0: encountered \"Lorem ipsum dolor sit amet, cons\""
    ));

    make_ready_future(())
});

teseladb::engine_test_case!(case_insensitive_header_reply, {
    let mut parser = HttpResponseParser::default();
    parser.init();
    let r101: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\ncontent-type: application/json\r\n\r\n{\"Hello\":\"World\"}";

    parse_all(&mut parser, r101);
    let response = parser.get_parsed_response().expect("response");
    let rep: Box<Reply> = Box::new(*response);
    assert_eq!(rep.get_header("content-length"), "17");
    assert_eq!(rep.get_header("Content-Length"), "17");
    assert_eq!(rep.get_header("cOnTeNT-lEnGTh"), "17");

    make_ready_future(())
});

teseladb::engine_test_case!(multiple_connections, {
    teseladb::engine::core::thread::async_(|| {
        let mut lcf = LoopbackConnectionFactory::with_pending_capacity(Smp::count() + 1, 1);
        let mut server = TestServer::new(&ss("test"));
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());
        let addr = any_addr();

        let mut socks: Vec<ConnectedSocket> = Vec::new();
        // Make sure one shard has two connections pending.
        for _ in 0..=Smp::count() {
            socks.push(
                LoopbackSocketImpl::new(&mut lcf, None)
                    .connect(addr, addr, Transport::TCP)
                    .get()?,
            );
        }

        server.pin().do_accepts(0).get()?;
        server.get().stop().get()?;
        lcf.destroy_all_shards().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(http_parse_response_status, {
    let mut parser = HttpResponseParser::default();
    parser.init();
    let r101: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\n\r\n";
    let r200: &[u8] = b"HTTP/1.1 200 OK\r\nHost: localhost\r\nhello\r\n";

    parse_all(&mut parser, r101);
    let mut response = parser.get_parsed_response().expect("response");
    assert_eq!(response._status, StatusType::SWITCHING_PROTOCOLS);

    parser.init();
    parse_all(&mut parser, r200);
    response = parser.get_parsed_response().expect("response");
    assert_eq!(response._status, StatusType::OK);
    make_ready_future(())
});

teseladb::engine_test_case!(http_parse_response_small_json, {
    let mut parser = HttpResponseParser::default();
    parser.init();
    let r101: &[u8] = b"HTTP/1.1 200 OK\r\ndate: Thu, 25 May 2023 16:13:59 GMT\r\nserver: uvicorn\r\ncontent-length: 17\r\ncontent-type: application/json\r\n\r\n{\"Hello\":\"World\"}";

    parse_all(&mut parser, r101);
    let resp = parser.get_parsed_response().expect("response");
    assert_eq!(resp._status.0, 200);
    assert_eq!(resp.get_header("Content-Length"), "17");
    assert_eq!(resp.get_header("Content-Type"), "application/json");
    assert_eq!(resp.get_header("server"), "uvicorn");
    assert_eq!(resp.get_header("date"), "Thu, 25 May 2023 16:13:59 GMT");

    make_ready_future(())
});

teseladb::engine_test_case!(test_shared_future, {
    let p = SharedPromise::<JsonReturnType>::new();
    let fut = p.get_shared_future();

    let _ = yield_().then(move |()| {
        let mut p = p;
        p.set_value(JsonReturnType::new(&JsonVoid as &dyn Jsonable).unwrap_or_default());
    });

    fut.discard_result()
});

teseladb::engine_test_case!(test_url_encode_decode, {
    let mut encoded: Sstring;
    let mut decoded = Sstring::new();
    let mut ok: bool;

    let all_valid = ss("~abcdefghijklmnopqrstuvwhyz-ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789.");
    encoded = url_internal::url_encode(all_valid.as_bytes());
    ok = url_internal::url_decode(encoded.as_bytes(), &mut decoded);
    assert!(ok);
    assert_eq!(decoded, all_valid);
    assert_eq!(all_valid, encoded);

    let some_invalid = ss("a?/!@#$%^&*()[]=.\\ \tZ");
    encoded = url_internal::url_encode(some_invalid.as_bytes());
    ok = url_internal::url_decode(encoded.as_bytes(), &mut decoded);
    assert!(ok);
    assert_eq!(decoded, some_invalid);
    for i in 0..encoded.length() {
        let c = encoded.as_bytes()[i];
        if c != b'%' && c != b'+' {
            let f = all_valid.as_bytes().iter().position(|&x| x == c);
            assert_ne!(f, None);
        }
    }

    make_ready_future(())
});

teseladb::engine_test_case!(test_url_param_encode_decode, {
    let mut to_send = Request { _url: ss("/foo/bar"), ..Request::default() };
    to_send.query_parameters.insert(ss("a"), ss("a+a*a"));
    to_send.query_parameters.insert(ss("b"), ss("b/b%b"));

    let mut to_recv = Request { _url: to_send.format_url(), ..Request::default() };
    let url = to_recv.parse_query_param();

    assert_eq!(url, to_send._url);
    assert_eq!(to_recv.query_parameters.len(), to_send.query_parameters.len());
    for (k, v) in &to_send.query_parameters {
        let it = to_recv.query_parameters.get(k);
        assert!(it.is_some());
        assert_eq!(it, Some(v));
    }

    make_ready_future(())
});

teseladb::engine_test_case!(test_unexpected_exception_format, {
    let ex = unexpected_status_error(StatusType::SEE_OTHER);
    assert_eq!(Sstring::from(ex.what()), ss(&format!("{}", StatusType::SEE_OTHER)));
    make_ready_future(())
});

teseladb::engine_test_case!(test_redirect_exception, {
    async_(|| -> Result<(), ExceptionPtr> {
        #[derive(Default)]
        struct PermHandle {
            base: HandlerBaseData,
        }
        impl HandlerBase for PermHandle {
            handler_base_data!();
            fn handle(
                &mut self,
                _path: &Sstring,
                _req: Box<Request>,
                _rep: Box<Reply>,
            ) -> Future<Box<Reply>> {
                make_exception_future(ExceptionPtr::new(redirect_exception_default("/perm_loc")))
            }
        }

        #[derive(Default)]
        struct TempHandle {
            base: HandlerBaseData,
        }
        impl HandlerBase for TempHandle {
            handler_base_data!();
            fn handle(
                &mut self,
                _path: &Sstring,
                _req: Box<Request>,
                _rep: Box<Reply>,
            ) -> Future<Box<Reply>> {
                make_exception_future(ExceptionPtr::new(redirect_exception(
                    "/temp_loc",
                    StatusType::MOVED_TEMPORARILY,
                )))
            }
        }

        let mut lcf = LoopbackConnectionFactory::new(1);
        let mut server = TestServer::new(&ss("test"));
        HttpServerTester::listeners(server.get()).push(lcf.get_server_socket());

        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let client = async_(move || -> Result<(), ExceptionPtr> {
            let mut cln = default_loopback_client(lcfp);

            let mut test = |path: Sstring,
                            expected_dest: Sstring,
                            expected_status: StatusType|
             -> Result<(), ExceptionPtr> {
                let req = Request::make(ss("GET"), ss("test"), path);
                let status: Rc<Cell<Option<StatusType>>> = Rc::new(Cell::new(None));
                let location: Rc<RefCell<Sstring>> = Rc::new(RefCell::new(Sstring::new()));
                let (st, loc) = (status.clone(), location.clone());
                cln.as_mut()
                    .make_request(
                        req,
                        reply_handler(move |rep: &Reply, _in: InputStream<u8>| {
                            st.set(Some(rep._status));
                            *loc.borrow_mut() = rep.get_header("Location");
                            make_ready_future(())
                        }),
                        None,
                    )
                    .get()?;
                assert!(status.get().is_some());
                assert_eq!(status.get(), Some(expected_status));
                assert_eq!(*location.borrow(), expected_dest);
                Ok(())
            };

            test(ss("/perm"), ss("/perm_loc"), StatusType::MOVED_PERMANENTLY)?;
            test(ss("/temp"), ss("/temp_loc"), StatusType::MOVED_TEMPORARILY)?;

            cln.as_mut().close().get()?;
            Ok(())
        });

        server.get()._routes.put(
            OperationType::Get,
            "/perm",
            raw_handler(PermHandle::default()),
        )?;
        server.get()._routes.put(
            OperationType::Get,
            "/temp",
            raw_handler(TempHandle::default()),
        )?;
        server.pin().do_accepts(0).get()?;
        client.get()?;
        server.get().stop().get()?;
        Ok(())
    })
});

#[test]
fn test_path_decode_unchanged() {
    let unchanged_chars = ss("~abcdefghijklmnopqrstuvwhyz-ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789.+");
    let mut result = Sstring::new();
    let success = url_internal::path_decode(unchanged_chars.as_bytes(), &mut result);

    assert!(success);
    assert_eq!(result, unchanged_chars);
}

#[test]
fn test_path_decode_changed() {
    let changed_chars = ss("%20");
    let mut result = Sstring::new();
    let success = url_internal::path_decode(changed_chars.as_bytes(), &mut result);

    assert!(success);

    let expected_chars = ss(" ");
    assert_eq!(result, expected_chars);
}

/// The name of a status class, for assertion messages.
fn status_class_name(sc: StatusClass) -> &'static str {
    const STATUS_STRINGS: [&str; 6] = [
        "1xx: Informational",
        "2xx: Success",
        "3xx: Redirection",
        "4xx: Client Error",
        "5xx: Server Error",
        "Unclassified",
    ];
    let status = (sc as u8 as u32).wrapping_sub(1);
    if (status as usize) < STATUS_STRINGS.len() {
        return STATUS_STRINGS[status as usize];
    }
    "Unclassified"
}

#[test]
fn test_http_status_classification() {
    let mut informational: usize = 0;
    let mut success: usize = 0;
    let mut redirection: usize = 0;
    let mut client_error: usize = 0;
    let mut server_error: usize = 0;
    let mut unclassified: usize = 0;
    for i in -100..700 {
        let classification = Reply::classify_status(StatusType(i));
        let check = |expected: StatusClass| {
            assert_eq!(
                classification,
                expected,
                "{} != {}",
                status_class_name(classification),
                status_class_name(expected)
            );
        };
        if (100..200).contains(&i) {
            informational += 1;
            check(StatusClass::Informational);
        } else if (200..300).contains(&i) {
            success += 1;
            check(StatusClass::Success);
        } else if (300..400).contains(&i) {
            redirection += 1;
            check(StatusClass::Redirection);
        } else if (400..500).contains(&i) {
            client_error += 1;
            check(StatusClass::ClientError);
        } else if (500..600).contains(&i) {
            server_error += 1;
            check(StatusClass::ServerError);
        } else {
            unclassified += 1;
            check(StatusClass::Unclassified);
        }
    }
    assert_eq!(informational, 100);
    assert_eq!(success, 100);
    assert_eq!(redirection, 100);
    assert_eq!(client_error, 100);
    assert_eq!(server_error, 100);
    assert_eq!(unclassified, 300);
}

// Check that trying a http connection with a wire error
// can be handled.
#[cfg(feature = "tls")]
teseladb::engine_test_case!(test_http_with_broken_wire, {
    teseladb::engine::core::thread::async_(|| {
        let b = tls::CredentialsBuilder::default();
        let creds = b.build_certificate_credentials()?;

        let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        opts.set_fixed_cpu(this_shard_id());

        let mut server = listen_with(make_ipv4_address_ip_port(0x7f000001, 0), opts)?;
        let addr = server.local_address()?;

        let mut c = Box::pin(Client::with_tls(addr, creds, Sstring::new()));
        let mut req = Request::default();

        req.write_body("html", Sstring::filled(5134, b'a'));

        let sa = server.accept();
        let f = c.as_mut().make_request(
            req,
            reply_handler(|_rep: &Reply, _body: InputStream<u8>| -> Future<()> {
                make_coroutine(async move {
                    // don't care
                    Ok(())
                })
            }),
            None,
        );

        let mut s = sa.get()?;

        s.connection.input(Default::default()).close().get()?;
        s.connection.output(8192).close().get()?;

        assert!(f.get().is_err_and(|e| is_system_error(&e)));

        c.as_mut().close().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_client_close_connection, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut lcf = LoopbackConnectionFactory::new(1);
        let lcfp: *mut LoopbackConnectionFactory = &mut lcf;
        let make_test_request = move || -> Result<(), ExceptionPtr> {
            let mut cln = loopback_client(lcfp, 1, RetryRequests::NO);
            let content_length = Rc::new(Cell::new(0usize));
            for _ in [1, 2] {
                let req = get_request("/test");
                let cl = content_length.clone();
                let make_request = cln.as_mut().make_request(
                    req,
                    reply_handler(move |resp: &Reply, in_: InputStream<u8>| {
                        cl.set(resp.content_length);
                        let cl = cl.clone();
                        let mut in_ = in_;
                        async_(move || -> Result<(), ExceptionPtr> {
                            // just read some bytes and abandon
                            let buff = in_.read().get()?;
                            assert!(buff.size() < cl.get());
                            in_.close().get()?;
                            Ok(())
                        })
                    }),
                    Some(StatusType::OK),
                );
                assert!(make_request.get().is_ok());
            }
            cln.as_mut().close().get()?;
            Ok(())
        };

        let response_size = Rc::new(Cell::new(0usize));
        let make_response = {
            let response_size = response_size.clone();
            move |ar: AcceptResult| -> Future<()> {
                let response_size = response_size.get();
                serve_connection(ar, move |sk| {
                    let mut in_ = sk.input(Default::default());
                    read_simple_http_request(&mut in_)?;
                    let mut out = sk.output(8192);
                    let mut responses: usize = 0;
                    // In the case the leftover data on the socket is smaller than 128KiB we are going to drain it and leave the connection alive, so here we
                    // have to loop two times to fulfill two request from the client. On the other hand if the leftover data is larger than 128KiB we are going
                    // to close the connection, so we have to loop only once to fulfill one request from the client and make another `accept`
                    loop {
                        if responses == 2 {
                            break;
                        }
                        responses += 1;
                        let r = (|| -> Result<(), ExceptionPtr> {
                            out.write_str(&format!(
                                "HTTP/1.1 200 OK\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                                response_size
                            ))
                            .get()?;
                            out.flush().get()?;
                            out.write_sstring(&Sstring::filled(response_size / 2, b'a')).get()?;
                            out.flush().get()?;

                            out.write_sstring(&Sstring::filled(response_size / 2, b'a')).get()?;
                            out.flush().get()?;
                            Ok(())
                        })();
                        if r.is_err() {
                            break;
                        }
                    }
                    out.close().get()?;
                    Ok(())
                })
            }
        };

        const KIB: usize = 1024;
        for size in [128 * KIB, 260 * KIB] {
            response_size.set(size);
            let mut ss_ = lcf.get_server_socket();
            let ssp: *mut _ = &mut ss_;
            let mut server = ss_.accept().then(make_response.clone());
            if size > 128 * KIB {
                // In this case the client is going to reset the connection so we have to `accept` again
                let mr = make_response.clone();
                // SAFETY: the server socket outlives the server chain, joined below.
                server = server.then(move |()| unsafe { (*ssp).accept() }.then(mr));
            }
            let mtr = make_test_request;
            let client = async_(mtr);

            when_all((server, client)).discard_result().get()?;
        }
        Ok(())
    })
});
