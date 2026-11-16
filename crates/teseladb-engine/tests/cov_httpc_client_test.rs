//! Tests of the HTTP client against servers written by hand over TCP: keep-alive
//! connections pooled and reused, replies of an older protocol version or with
//! `Connection: close`, content lengths with a sign, an unexpected status whose body is read
//! for the debug log, the connection limit raised and lowered (also while a request holds a
//! connection), requests waiting for a free connection, a bare connection, a request whose
//! body is missing, an early reply to `Expect: 100-continue`, requests by reference, and a
//! handler error that is not retried.
//!
//! The raw servers run in plain threads with blocking sockets; the client runs in an engine
//! thread that polls for them.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::io::Read as _;
use std::io::Write as _;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::iostream::InputStream;
use teseladb::engine::core::reactor::connect;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::client::experimental::{
    Client, Connection, ReplyHandler, RetryRequests,
};
use teseladb::engine::http::client::internal::ClientRef;
use teseladb::engine::http::connection_factory::experimental::BasicConnectionFactory;
use teseladb::engine::http::exception::{BaseException, ExceptionKind};
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine::net::api::make_ipv4_address_ip_port;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine::util::log::{LogLevel, Logger, Ostream, global_logger_registry};
use teseladb::engine::util::short_streams::util::read_entire_stream_contiguous;

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov_httpc_client_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

// ---- logs ----

static LOG_BUF: LazyLock<Arc<Mutex<Vec<u8>>>> = LazyLock::new(|| Arc::new(Mutex::new(Vec::new())));

struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut v) => v.extend_from_slice(b),
            Err(p) => p.into_inner().extend_from_slice(b),
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    Logger::set_ostream(Ostream::Custom(Box::new(SharedBuf(LOG_BUF.clone()))));
}

fn logged(s: &str) -> bool {
    let v = match LOG_BUF.lock() {
        Ok(v) => v.clone(),
        Err(p) => p.into_inner().clone(),
    };
    String::from_utf8_lossy(&v).contains(s)
}

fn exc_what(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
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

// ---- the raw server ----

type PeerResult<T> = Result<T, String>;

fn io<T>(r: std::io::Result<T>) -> PeerResult<T> {
    r.map_err(|e| e.to_string())
}

/// Reads a request head (through the blank line) and its body; `None` at the end of the
/// connection.
fn read_request(s: &mut TcpStream) -> PeerResult<Option<String>> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match s.read(&mut b) {
            Ok(0) => return Ok(None),
            Ok(_) => head.push(b[0]),
            Err(e) => return Err(e.to_string()),
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let len = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    if len > 0 && !text.to_ascii_lowercase().contains("expect: 100-continue") {
        let mut body = vec![0u8; len];
        io(s.read_exact(&mut body))?;
    }
    Ok(Some(text))
}

/// Reads until the client closes the connection.
fn drain(s: &mut TcpStream) -> PeerResult<()> {
    let mut v = Vec::new();
    // A reset from the client also ends the connection.
    let _ = s.read_to_end(&mut v);
    Ok(())
}

fn send(s: &mut TcpStream, reply: &str) -> PeerResult<()> {
    io(s.write_all(reply.as_bytes()))?;
    io(s.flush())
}

fn listener() -> Result<(TcpListener, SocketAddress), ExceptionPtr> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(ExceptionPtr::from)?;
    let p = l.local_addr().map_err(ExceptionPtr::from)?.port();
    Ok((l, make_ipv4_address_ip_port(0x7f00_0001, p)))
}

/// What the server does with one accepted connection.
type Script = Box<dyn FnOnce(&mut TcpStream) -> PeerResult<()> + Send>;

/// Serves the connections `l` accepts one after the other, each with its script.
fn sequential_server(l: TcpListener, scripts: Vec<Script>) -> JoinHandle<PeerResult<()>> {
    std::thread::spawn(move || {
        for script in scripts {
            let (mut s, _) = io(l.accept())?;
            io(s.set_read_timeout(Some(Duration::from_secs(10))))?;
            script(&mut s)?;
        }
        Ok(())
    })
}

/// Serves every connection `l` accepts in its own thread, until `stop` is set: each request
/// gets `200 ok`, after 300 ms for the path `/slow`. Returns how many connections it accepted.
fn pool_server(l: TcpListener, stop: Arc<AtomicBool>) -> JoinHandle<PeerResult<usize>> {
    std::thread::spawn(move || {
        io(l.set_nonblocking(true))?;
        let mut workers = Vec::new();
        let mut accepted = 0;
        while !stop.load(Ordering::Relaxed) {
            match l.accept() {
                Ok((mut s, _)) => {
                    accepted += 1;
                    io(s.set_nonblocking(false))?;
                    io(s.set_read_timeout(Some(Duration::from_secs(10))))?;
                    workers.push(std::thread::spawn(move || -> PeerResult<()> {
                        while let Some(head) = read_request(&mut s)? {
                            if head.starts_with("GET /slow") {
                                std::thread::sleep(Duration::from_millis(300));
                            }
                            send(&mut s, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")?;
                        }
                        Ok(())
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        for w in workers {
            match w.join() {
                Ok(r) => r?,
                Err(_) => return Err("a connection thread panicked".to_string()),
            }
        }
        Ok(accepted)
    })
}

/// Waits, from an engine thread, for the server thread `h` and returns its result.
fn join<T>(h: JoinHandle<PeerResult<T>>) -> Result<T, ExceptionPtr> {
    for _ in 0..1500 {
        if h.is_finished() {
            break;
        }
        sleep(Duration::from_millis(10)).get()?;
    }
    assert!(h.is_finished(), "the raw server did not finish");
    match h.join() {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => panic!("raw server failed: {}", e),
        Err(_) => panic!("raw server panicked"),
    }
}

// ---- the client side ----

fn get(path: &str) -> Request {
    Request::make(Sstring::from("GET"), Sstring::from("test"), Sstring::from(path))
}

/// What the replies handled so far were: status and body.
type Seen = Rc<RefCell<Vec<(StatusType, String)>>>;

/// A handler that reads the whole body and records it with the status.
fn collector(seen: &Seen) -> ReplyHandler {
    let seen = seen.clone();
    ReplyHandler::new(move |rep: &Reply, body: InputStream<u8>| -> Future<()> {
        let status = rep._status;
        let seen = seen.clone();
        do_with(body, move |b: &mut InputStream<u8>| {
            // SAFETY: do_with keeps the stream alive and in place until the read resolves.
            unsafe { read_entire_stream_contiguous(b) }.then(move |s: Sstring| {
                seen.borrow_mut()
                    .push((status, String::from_utf8_lossy(s.as_bytes()).into_owned()));
                make_ready_future(())
            })
        })
    })
}

fn bodies(seen: &Seen) -> Vec<String> {
    seen.borrow().iter().map(|(_, b)| b.clone()).collect()
}

teseladb::engine_test_case!(the_client_pools_keep_alive_connections_and_drops_old_ones, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (l, addr) = listener()?;
        let server = sequential_server(
            l,
            vec![
                Box::new(|s: &mut TcpStream| {
                    read_request(s)?;
                    // A content length with a sign.
                    send(s, "HTTP/1.1 200 OK\r\nContent-Length: +3\r\n\r\nabc")?;
                    read_request(s)?;
                    send(s, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nde")?;
                    read_request(s)?;
                    // An older protocol: the connection is not kept.
                    send(s, "HTTP/1.0 200 OK\r\nContent-Length: 1\r\n\r\nf")?;
                    drain(s)
                }),
                Box::new(|s: &mut TcpStream| {
                    read_request(s)?;
                    send(s, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 1\r\n\r\ng")?;
                    drain(s)
                }),
            ],
        );
        let mut cl = Box::pin(Client::new(addr));
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        cl.as_mut().make_request(get("/1"), collector(&seen), Some(StatusType::OK)).get()?;
        cl.as_mut().make_request(get("/2"), collector(&seen), Some(StatusType::OK)).get()?;
        assert_eq!(cl.total_new_connections_nr(), 1);
        assert_eq!(cl.connections_nr(), 1);
        assert_eq!(cl.idle_connections_nr(), 1);
        cl.as_mut().make_request(get("/3"), collector(&seen), Some(StatusType::OK)).get()?;
        assert_eq!(cl.idle_connections_nr(), 0);
        wait_until("the old connection to close", || cl.connections_nr() == 0)?;
        cl.as_mut().make_request(get("/4"), collector(&seen), Some(StatusType::OK)).get()?;
        assert_eq!(cl.total_new_connections_nr(), 2);
        assert_eq!(cl.idle_connections_nr(), 0);
        wait_until("the closed connection to go", || cl.connections_nr() == 0)?;
        assert_eq!(bodies(&seen), ["abc", "de", "f", "g"]);
        cl.as_mut().close().get()?;
        join(server)?;
        Ok(())
    })
});

teseladb::engine_test_case!(an_unexpected_status_is_read_for_the_debug_log, {
    async_(|| -> Result<(), ExceptionPtr> {
        capture_logs();
        let (l, addr) = listener()?;
        let server = sequential_server(
            l,
            vec![Box::new(|s: &mut TcpStream| {
                read_request(s)?;
                send(s, "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx")?;
                read_request(s)?;
                send(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\nnope")?;
                drain(s)
            })],
        );
        let mut cl = Box::pin(Client::new(addr));
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        // The first request makes the client's logger exist.
        cl.as_mut().make_request(get("/ok"), collector(&seen), Some(StatusType::OK)).get()?;
        let http = Sstring::from("http");
        let before = global_logger_registry().get_logger_level(&http).unwrap_or(LogLevel::Info);
        global_logger_registry().set_logger_level(&http, LogLevel::Debug)?;
        let r =
            cl.as_mut().make_request(get("/missing"), collector(&seen), Some(StatusType::OK)).get();
        global_logger_registry().set_logger_level(&http, before)?;
        assert!(r.is_err_and(|e| {
            e.downcast_ref::<BaseException>()
                .is_some_and(|b| *b.kind() == ExceptionKind::UnexpectedStatus)
        }));
        assert!(logged("request finished with"));
        assert!(logged("nope"));
        // The handler never saw the unexpected reply.
        assert_eq!(bodies(&seen), ["x"]);
        cl.as_mut().close().get()?;
        join(server)?;
        Ok(())
    })
});

teseladb::engine_test_case!(the_connection_limit_grows_and_shrinks, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (l, addr) = listener()?;
        let stop = Arc::new(AtomicBool::new(false));
        let server = pool_server(l, stop.clone());
        let mut cl = Box::pin(Client::with_factory(
            Box::new(BasicConnectionFactory::new(addr)),
            2,
            RetryRequests::NO,
            Client::DEFAULT_MAX_BYTES_TO_DRAIN,
        ));
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        // Two requests at once take two connections, which then wait in the pool.
        let a = cl.as_mut().make_request(get("/a"), collector(&seen), Some(StatusType::OK));
        let b = cl.as_mut().make_request(get("/b"), collector(&seen), Some(StatusType::OK));
        a.get()?;
        b.get()?;
        assert_eq!(cl.connections_nr(), 2);
        assert_eq!(cl.idle_connections_nr(), 2);

        // A lower limit closes pooled connections; a higher one changes nothing.
        cl.as_mut().set_maximum_connections(1).get()?;
        assert!(cl.connections_nr() <= 1);
        let raised = cl.as_mut().set_maximum_connections(3);
        assert!(raised.available());
        raised.get()?;
        cl.as_mut().set_maximum_connections(1).get()?;

        // With one connection allowed, the second request waits for the first.
        let slow = cl.as_mut().make_request(get("/slow"), collector(&seen), Some(StatusType::OK));
        let fast = cl.as_mut().make_request(get("/fast"), collector(&seen), Some(StatusType::OK));
        slow.get()?;
        fast.get()?;
        assert!(cl.connections_nr() <= 1);

        // Lowering the limit while a request holds the only connection waits for it.
        let held = cl.as_mut().make_request(get("/slow"), collector(&seen), Some(StatusType::OK));
        let lowered = cl.as_mut().set_maximum_connections(0);
        held.get()?;
        lowered.get()?;
        wait_until("every connection to close", || cl.connections_nr() == 0)?;
        assert_eq!(cl.idle_connections_nr(), 0);
        assert_eq!(seen.borrow().len(), 5);
        assert!(seen.borrow().iter().all(|(st, body)| *st == StatusType::OK && body == "ok"));
        cl.as_mut().set_maximum_connections(2).get()?;
        cl.as_mut().close().get()?;
        stop.store(true, Ordering::Relaxed);
        let accepted = join(server)?;
        assert!(accepted >= 2);
        Ok(())
    })
});

teseladb::engine_test_case!(a_bare_connection_makes_a_request, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (l, addr) = listener()?;
        let server = sequential_server(
            l,
            vec![Box::new(|s: &mut TcpStream| {
                read_request(s)?;
                send(s, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")?;
                drain(s)
            })],
        );
        let mut cl = Box::pin(Client::new(addr));
        let fd = connect(addr).get()?;
        // SAFETY: the client is pinned and outlives the connection, which the test drops (and
        // waits to be counted out) before closing the client.
        let cr = unsafe { ClientRef::new(cl.as_mut().get_unchecked_mut()) };
        assert_eq!(cl.connections_nr(), 1);
        let con = Connection::new(fd, cr);
        // SAFETY: the connection lives in `con` and only this thread uses it.
        let mut rep = unsafe { con.get_mut_unchecked() }.make_request(get("/bare")).get()?;
        assert!(rep._status == StatusType::OK);
        assert_eq!(rep.content_length, 2);
        // SAFETY: as above; `rep` and the stream live in this frame, the stream dropped first.
        let mut body = unsafe { con.get_mut_unchecked().in_(&mut rep) };
        // SAFETY: `body` lives until the read resolves.
        let text = unsafe { read_entire_stream_contiguous(&mut body) }.get()?;
        assert_eq!(text, "hi");
        drop(body);
        // SAFETY: as above.
        unsafe { con.get_mut_unchecked() }.close().get()?;
        drop(con);
        wait_until("the connection to be counted out", || cl.connections_nr() == 0)?;
        cl.as_mut().close().get()?;
        join(server)?;
        Ok(())
    })
});

teseladb::engine_test_case!(a_request_without_its_body_fails_and_an_early_reply_is_returned, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (l, addr) = listener()?;
        let server = sequential_server(
            l,
            vec![
                // The request fails before anything is sent.
                Box::new(|s: &mut TcpStream| {
                    if read_request(s)?.is_some() {
                        return Err("a request without its body was sent".to_string());
                    }
                    Ok(())
                }),
                // The server refuses the body before it is sent.
                Box::new(|s: &mut TcpStream| {
                    let head = read_request(s)?.ok_or("no request")?;
                    if !head.to_ascii_lowercase().contains("expect: 100-continue") {
                        return Err(format!("no expectation in {:?}", head));
                    }
                    send(s, "HTTP/1.1 417 Expectation Failed\r\nContent-Length: 0\r\n\r\n")?;
                    io(s.shutdown(Shutdown::Write))?;
                    drain(s)
                }),
            ],
        );
        let mut cl = Box::pin(Client::new(addr));
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        let mut post =
            Request::make(Sstring::from("POST"), Sstring::from("test"), Sstring::from("/x"));
        post.content_length = 5;
        let r = cl.as_mut().make_request(post, collector(&seen), None).get();
        assert!(
            r.is_err_and(|e| exc_what(&e) == "Request body writer not set and content is empty")
        );
        wait_until("the failed connection to close", || cl.connections_nr() == 0)?;

        let mut put =
            Request::make(Sstring::from("PUT"), Sstring::from("test"), Sstring::from("/y"));
        put.write_body("txt", Sstring::from("data!"));
        put.set_expects_continue();
        cl.as_mut().make_request(put, collector(&seen), None).get()?;
        assert_eq!(seen.borrow().len(), 1);
        assert!(seen.borrow()[0].0 == StatusType::EXPECTATION_FAILED);
        cl.as_mut().close().get()?;
        wait_until("every connection to close", || cl.connections_nr() == 0)?;
        join(server)?;
        Ok(())
    })
});

teseladb::engine_test_case!(requests_by_reference_and_errors_that_are_not_retried, {
    async_(|| -> Result<(), ExceptionPtr> {
        let (l, addr) = listener()?;
        let server = sequential_server(
            l,
            vec![Box::new(|s: &mut TcpStream| {
                read_request(s)?;
                send(s, "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc")?;
                read_request(s)?;
                send(s, "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ndef")?;
                drain(s)
            })],
        );
        let mut cl = Box::pin(Client::with_factory(
            Box::new(BasicConnectionFactory::new(addr)),
            Client::DEFAULT_MAX_CONNECTIONS,
            RetryRequests::YES,
            Client::DEFAULT_MAX_BYTES_TO_DRAIN,
        ));
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        let mut req = get("/ref");
        let mut handler = collector(&seen);
        // SAFETY: the request and the handler live in this frame until the call resolves.
        unsafe {
            cl.as_mut().make_request_ref(
                &mut req,
                &mut handler,
                Some(StatusType::OK),
                std::ptr::null_mut(),
            )
        }
        .get()?;
        assert_eq!(bodies(&seen), ["abc"]);

        // A handler error is not a connection error: it is returned, not retried.
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let failing =
            ReplyHandler::new(move |_rep: &Reply, _body: InputStream<u8>| -> Future<()> {
                c.set(c.get() + 1);
                make_exception_future(ExceptionPtr::new(RuntimeError("handler failed".to_string())))
            });
        let r = cl.as_mut().make_request(get("/fail"), failing, Some(StatusType::OK)).get();
        assert!(r.is_err_and(|e| exc_what(&e) == "handler failed"));
        assert_eq!(calls.get(), 1);
        assert_eq!(cl.total_new_connections_nr(), 1);
        wait_until("the failed connection to close", || cl.connections_nr() == 0)?;
        cl.as_mut().close().get()?;
        join(server)?;
        Ok(())
    })
});
