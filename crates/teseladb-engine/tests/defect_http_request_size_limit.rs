#![allow(unsafe_code)]
#![cfg(target_os = "linux")]
//! The HTTP server bounds the request line and headers together (32 KiB by default): once a
//! request head goes past it, the server answers 431 Request Header Fields Too Large (or 414
//! URI Too Long when the request line is what overflowed) and closes, without waiting for the
//! end of the head. Before, the server buffered request heads of any size, so a client could
//! make it hold unbounded memory by never ending one.
//!
//! The oversized heads are sent unfinished, in two writes: the server reads all of the first
//! and waits, then reads the second, which takes it past the limit. It has then read every
//! byte sent, so its close is a FIN and not a reset that could drop the reply.

use std::cell::Cell;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::connect;
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{SteadyClockType, Timer};
use teseladb::engine::http::common::httpd::OperationType;
use teseladb::engine::http::function_handlers::FunctionHandler;
use teseladb::engine::http::httpd::httpd::{HttpServer, HttpServerTester};
use teseladb::engine::http::request::Request;
use teseladb::engine::net::api::{ConnectedSocket, ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine_test_case;

/// The server's default limit on the request line and headers.
const DEFAULT_LIMIT: usize = 32 * 1024;

/// How long the client waits for a status line.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Sends `first`, then (once the server has read it) `rest`, on a new connection to `addr`,
/// and returns the status line of the reply; "timed out" when none comes.
fn status_line(addr: SocketAddress, first: &str, rest: &str) -> Result<String, ExceptionPtr> {
    let mut socket = connect(addr).get()?;
    let mut out = socket.output(65536);
    let mut input = socket.input(Default::default());
    out.write_str(first).get()?;
    out.flush().get()?;
    if !rest.is_empty() {
        sleep(Duration::from_millis(100)).get()?;
        out.write_str(rest).get()?;
        out.flush().get()?;
    }
    // No reply in time: the connection's input is shut down, which ends the read below.
    let timed_out = Rc::new(Cell::new(false));
    let socketp: *mut ConnectedSocket = &mut socket;
    let flag = timed_out.clone();
    let mut timer = pin!(Timer::<SteadyClockType>::new());
    timer.as_mut().set_callback(move || {
        flag.set(true);
        // SAFETY: the socket outlives the timer, cancelled below before the socket goes.
        unsafe { (*socketp).shutdown_input() };
    });
    timer.as_mut().arm_in(REPLY_TIMEOUT);
    let mut reply = Vec::new();
    while !reply.windows(2).any(|w| w == b"\r\n") {
        match input.read().get() {
            Ok(b) if !b.empty() => reply.extend_from_slice(b.as_slice()),
            _ => break,
        }
    }
    timer.as_mut().cancel();
    // The server closes the connection after an error reply: closing may then fail.
    let _ = out.close().get();
    let _ = input.close().get();
    if timed_out.get() {
        return Ok("timed out".to_string());
    }
    let text = String::from_utf8_lossy(&reply).into_owned();
    Ok(text.lines().next().unwrap_or("").to_string())
}

/// The status line of a request head of `head_start` followed by filler, sent unfinished:
/// `DEFAULT_LIMIT - 100` bytes first, then 300 more.
fn oversized_status(addr: SocketAddress, head_start: &str) -> Result<String, ExceptionPtr> {
    let first_len = DEFAULT_LIMIT - 100;
    let mut first = head_start.to_string();
    first.push_str(&"x".repeat(first_len - head_start.len()));
    status_line(addr, &first, &"x".repeat(300))
}

engine_test_case!(request_size_limit, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut server = Box::pin(HttpServer::new(&Sstring::from("test")));
        server.as_mut().start();
        // SAFETY: one field access that does not move the server out of its box.
        let srv = unsafe { Pin::get_unchecked_mut(server.as_mut()) };
        srv._routes.put(
            OperationType::Get,
            "/test",
            Box::new(FunctionHandler::from_request_function(
                Box::new(|_req: &Request| Ok(Sstring::from("hello"))),
                &Sstring::from("txt"),
            )),
        )?;
        // Every connection to this shard: with several shards a listener spreads them.
        let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        lo.set_fixed_cpu(this_shard_id());
        server.as_mut().listen_opts(make_ipv4_address_ip_port(0x7f00_0001, 0), lo).get()?;
        // SAFETY: as above.
        let srv = unsafe { Pin::get_unchecked_mut(server.as_mut()) };
        let addr = HttpServerTester::listeners(srv)[0].local_address()?;

        let lines = (|| -> Result<[String; 3], ExceptionPtr> {
            Ok([
                status_line(addr, "GET /test HTTP/1.1\r\nHost: test\r\n\r\n", "")?,
                oversized_status(addr, "GET /test HTTP/1.1\r\nHost: test\r\nX-Big: ")?,
                oversized_status(addr, "GET /")?,
            ])
        })();
        // SAFETY: as above.
        unsafe { Pin::get_unchecked_mut(server.as_mut()) }.stop().get()?;
        let [small, big_header, big_uri] = lines?;
        assert!(small.contains("200"), "a small request got: {small}");
        assert!(big_header.contains("431"), "oversized headers got: {big_header}");
        assert!(big_uri.contains("414"), "an oversized URI got: {big_uri}");
        Ok(())
    })
});
