//! Tests of the HTTP server over real TCP connections: the control object that runs a
//! server on every shard and listens with options, without and with TLS; a single server
//! with its TLS credentials set beforehand; listen errors with and without TLS; a request
//! that cannot be parsed, a content length with a sign, a streamed body the handler does not
//! read, and a reply whose body writer fails.
//!
//! Plain requests come from plain threads with blocking sockets; TLS requests come from the
//! engine's HTTP client. The reactor runs two shards (set before `main`).
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

#[path = "common/tls_certs.rs"]
mod tls_certs;

use std::cell::RefCell;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpStream;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::shared_ptr::SharedPtr;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::client::experimental::{Client, ReplyHandler};
use teseladb::engine::http::common::httpd::OperationType;
use teseladb::engine::http::function_handlers::FunctionHandler;
use teseladb::engine::http::httpd::httpd::{HttpServer, HttpServerControl};
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine::http::routes::Routes;
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::net::tls::{
    self, AbstractCredentials, CertificateCredentials, ServerCredentials, X509CrtFormat,
};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::short_streams::util::read_entire_stream_contiguous;
use tls_certs::certfile;

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_httpc_server_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

const LOCALHOST: u32 = 0x7f00_0001;
/// An address of no local interface (192.0.2.1).
const FOREIGN: u32 = 0xC000_0201;

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
}

fn cf(file: &str) -> Sstring {
    Sstring::from(certfile(file).as_str())
}

fn free_port() -> Result<u16, ExceptionPtr> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(ExceptionPtr::from)?;
    let p = l.local_addr().map_err(ExceptionPtr::from)?.port();
    Ok(p)
}

/// Polls `cond` every 10 ms for up to 5 s, from an engine thread.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) -> Result<(), ExceptionPtr> {
    for _ in 0..500 {
        if cond() {
            return Ok(());
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    panic!("timed out waiting for {}", what);
}

type PeerResult<T> = Result<T, String>;

/// Sends `request` over a new connection to `port` and returns everything the server sends
/// until it closes the connection.
fn raw_exchange(port: u16, request: Vec<u8>) -> JoinHandle<PeerResult<String>> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        s.write_all(&request).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
}

/// Waits, from an engine thread, for the peer thread `h` and returns its result.
fn join<T>(h: JoinHandle<PeerResult<T>>) -> Result<T, ExceptionPtr> {
    for _ in 0..1000 {
        if h.is_finished() {
            break;
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    assert!(h.is_finished(), "the raw client did not finish");
    match h.join() {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => panic!("raw client failed: {}", e),
        Err(_) => panic!("raw client panicked"),
    }
}

/// How many requests the `/hello` handlers saw over TLS, on any shard.
static HTTPS_SEEN: AtomicU32 = AtomicU32::new(0);

/// Adds `/hello`, which answers `hello` and counts the requests that came over TLS.
fn add_hello(r: &mut Routes) {
    let h = FunctionHandler::from_handle_function(
        Box::new(|req: &Request, _rep: &mut Reply| -> Result<Sstring, ExceptionPtr> {
            if req.get_protocol_name() == "https" {
                HTTPS_SEEN.fetch_add(1, Ordering::Relaxed);
            }
            Ok(ss("hello"))
        }),
        &ss("txt"),
    );
    // The route is new on this server.
    let _ = r.put(OperationType::Get, "/hello", Box::new(h));
}

/// Client and server credentials over the generated test certificate and its CA.
fn credentials()
-> Result<(SharedPtr<CertificateCredentials>, SharedPtr<ServerCredentials>), ExceptionPtr> {
    let mut b = tls::CredentialsBuilder::default();
    b.set_x509_key_file(&cf("test.crt"), &cf("test.key"), X509CrtFormat::PEM).get()?;
    b.set_x509_trust_file(&cf("catest.pem"), X509CrtFormat::PEM).get()?;
    b.set_dh_level(tls::DhLevel::default());
    Ok((b.build_certificate_credentials()?, b.build_server_credentials()?))
}

/// A GET of `/hello` over TLS to `addr` with the engine's client; returns the body.
fn https_hello(
    addr: SocketAddress,
    creds: SharedPtr<CertificateCredentials>,
) -> Result<String, ExceptionPtr> {
    let mut c = Box::pin(Client::with_tls(addr, creds, ss("test.example.org")));
    let got = Rc::new(RefCell::new(String::new()));
    let g = got.clone();
    let handler = ReplyHandler::new(move |_rep: &Reply, body: InputStream<u8>| -> Future<()> {
        let g = g.clone();
        do_with(body, move |b: &mut InputStream<u8>| {
            // SAFETY: do_with keeps the stream alive and in place until the read resolves.
            unsafe { read_entire_stream_contiguous(b) }.then(move |s: Sstring| {
                *g.borrow_mut() = String::from_utf8_lossy(s.as_bytes()).into_owned();
                make_ready_future(())
            })
        })
    });
    let req = Request::make(ss("GET"), ss("test.example.org"), ss("/hello"));
    let r = c.as_mut().make_request(req, handler, Some(StatusType::OK)).get();
    c.as_mut().close().get()?;
    r?;
    let body = got.borrow().clone();
    Ok(body)
}

fn plain_get(path: &str) -> Vec<u8> {
    format!("GET {} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n", path).into_bytes()
}

teseladb::engine_test_case!(a_server_control_listens_on_every_shard_with_and_without_tls, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (client_creds, server_creds) = credentials()?;
        let mut control = HttpServerControl::default();
        control.start(&HttpServerControl::generate_server_name()).get()?;
        control.set_routes(add_hello).get()?;

        let p1 = free_port()?;
        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        control.listen_opts(make_ipv4_address_ip_port(LOCALHOST, p1), lo.clone()).get()?;
        let p2 = free_port()?;
        control.listen_with(make_ipv4_address_ip_port(LOCALHOST, p2), lo, None).get()?;
        let p3 = free_port()?;
        let tls_addr = make_ipv4_address_ip_port(LOCALHOST, p3);
        control.listen_creds(tls_addr, Some(server_creds)).get()?;

        for port in [p1, p2] {
            let reply = join(raw_exchange(port, plain_get("/hello")))?;
            assert!(reply.starts_with("HTTP/1.1 200 OK"), "{}", reply);
            assert!(reply.ends_with("hello"), "{}", reply);
        }
        let before = HTTPS_SEEN.load(Ordering::Relaxed);
        assert_eq!(https_hello(tls_addr, client_creds)?, "hello");
        assert_eq!(HTTPS_SEEN.load(Ordering::Relaxed), before + 1);
        control.stop().get()?;
        Ok(())
    })
});

/// A server on this shard only, its listeners sending every connection here.
fn fixed_options() -> ListenOptions {
    let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    lo.set_fixed_cpu(this_shard_id());
    lo
}

/// Sets the credentials every later listener of `s` uses (the older way to serve HTTPS).
#[allow(deprecated)]
fn set_credentials(s: &mut HttpServer, creds: SharedPtr<ServerCredentials>) {
    s.set_tls_credentials(Some(creds));
}

fn server_mut(s: &mut Pin<Box<HttpServer>>) -> &mut HttpServer {
    // SAFETY: the server is never moved out of its pin.
    unsafe { s.as_mut().get_unchecked_mut() }
}

teseladb::engine_test_case!(a_server_with_credentials_set_beforehand_serves_https, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (client_creds, server_creds) = credentials()?;
        let mut s = Box::pin(HttpServer::new(&ss("cov-https")));
        s.as_mut().start();
        add_hello(&mut server_mut(&mut s)._routes);

        // Listening on an address of no local interface fails, without and with TLS.
        let foreign = make_ipv4_address_ip_port(FOREIGN, free_port()?);
        assert!(s.as_mut().listen_creds(foreign, None).get().is_err());
        assert!(s.as_mut().listen_creds(foreign, Some(server_creds.clone())).get().is_err());

        set_credentials(server_mut(&mut s), server_creds);
        let addr = make_ipv4_address_ip_port(LOCALHOST, free_port()?);
        s.as_mut().listen_opts(addr, fixed_options()).get()?;
        let before = HTTPS_SEEN.load(Ordering::Relaxed);
        assert_eq!(https_hello(addr, client_creds)?, "hello");
        assert_eq!(HTTPS_SEEN.load(Ordering::Relaxed), before + 1);
        assert_eq!(s.requests_served(), 1);
        server_mut(&mut s).stop().get()?;
        Ok(())
    })
});

teseladb::engine_test_case!(a_server_handles_bad_requests_unread_bodies_and_failing_writers, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut s = Box::pin(HttpServer::new(&ss("cov-errors")));
        s.as_mut().start();
        server_mut(&mut s).set_content_streaming(true);
        let routes = &mut server_mut(&mut s)._routes;
        // Echoes the request body, read from its stream.
        routes.put(
            OperationType::Post,
            "/echo",
            Box::new(FunctionHandler::from_future_handler_function(
                Rc::new(|mut req: Box<Request>, mut rep: Box<Reply>| -> Future<Box<Reply>> {
                    let stream = req.content_stream;
                    // SAFETY: the server keeps the content stream alive while the handler runs.
                    unsafe { read_entire_stream_contiguous(&mut *stream) }.then(
                        move |body: Sstring| {
                            rep._content = body;
                            req.content = Sstring::new();
                            make_ready_future(rep)
                        },
                    )
                }),
                &ss("txt"),
            )),
        )?;
        // Answers without reading the request body.
        routes.put(
            OperationType::Post,
            "/ignore",
            Box::new(FunctionHandler::from_handle_function(
                Box::new(|_req: &Request, _rep: &mut Reply| -> Result<Sstring, ExceptionPtr> {
                    Ok(ss("ignored"))
                }),
                &ss("txt"),
            )),
        )?;
        // A reply whose body writer fails.
        routes.put(
            OperationType::Get,
            "/broken",
            Box::new(FunctionHandler::from_future_handler_function(
                Rc::new(|_req: Box<Request>, mut rep: Box<Reply>| -> Future<Box<Reply>> {
                    rep.write_body_with("txt", |_out: &mut OutputStream<u8>| -> Future<()> {
                        make_exception_future(ExceptionPtr::new(RuntimeError(
                            "the body cannot be written".to_string(),
                        )))
                    });
                    make_ready_future(rep)
                }),
                &ss("txt"),
            )),
        )?;
        let port = free_port()?;
        s.as_mut()
            .listen_with(make_ipv4_address_ip_port(LOCALHOST, port), fixed_options(), None)
            .get()?;

        // A request line that cannot be parsed: the reply uses version 1.1.
        let reply = join(raw_exchange(port, b"GARBAGE\r\n\r\n".to_vec()))?;
        assert!(reply.starts_with("HTTP/1.1 400 Bad Request"), "{}", reply);

        // A content length written with a sign.
        let reply = join(raw_exchange(
            port,
            b"POST /echo HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: +5\r\n\r\nhello"
                .to_vec(),
        ))?;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{}", reply);
        assert!(reply.ends_with("hello"), "{}", reply);

        // A body the handler leaves unread: the server closes the kept-alive connection.
        let reply = join(raw_exchange(
            port,
            b"POST /ignore HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\n0123456789".to_vec(),
        ))?;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{}", reply);
        assert!(reply.ends_with("ignored"), "{}", reply);

        // A body writer that fails: the reply is cut and the connection closed.
        let errors = s.reply_errors();
        let reply = join(raw_exchange(port, b"GET /broken HTTP/1.1\r\nHost: t\r\n\r\n".to_vec()))?;
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{}", reply);
        assert!(reply.to_ascii_lowercase().contains("transfer-encoding: chunked"), "{}", reply);
        wait_until("the reply error to be counted", || s.reply_errors() > errors)?;
        wait_until("every connection to close", || s.current_connections() == 0)?;
        server_mut(&mut s).stop().get()?;
        Ok(())
    })
});
