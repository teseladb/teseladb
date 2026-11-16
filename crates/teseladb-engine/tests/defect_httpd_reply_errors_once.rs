#![cfg(target_os = "linux")]
//! The HTTP server's `reply_errors` metric counts one error for a reply whose body writer
//! fails, not two.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::pin::Pin;
use std::rc::Rc;
use std::thread::JoinHandle;
use std::time::Duration;

use teseladb::engine::core::future::{
    ExceptionPtr, Future, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::iostream::OutputStream;
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::common::httpd::OperationType;
use teseladb::engine::http::function_handlers::FunctionHandler;
use teseladb::engine::http::httpd::httpd::HttpServer;
use teseladb::engine::http::reply::Reply;
use teseladb::engine::http::request::Request;
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};

const LOCALHOST: u32 = 0x7f00_0001;

fn ss(s: &str) -> Sstring {
    Sstring::from(s)
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

/// Sends `request` over a new connection to `port` and returns what the server sends until
/// it closes the connection.
fn raw_exchange(port: u16, request: &'static [u8]) -> JoinHandle<Result<String, String>> {
    std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        s.write_all(request).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
}

fn server_mut(s: &mut Pin<Box<HttpServer>>) -> &mut HttpServer {
    // SAFETY: the server is never moved out of its pin.
    unsafe { s.as_mut().get_unchecked_mut() }
}

teseladb::engine_test_case!(a_failing_body_writer_counts_one_reply_error, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut s = Box::pin(HttpServer::new(&ss("defect-reply-errors")));
        s.as_mut().start();
        server_mut(&mut s)._routes.put(
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
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(ExceptionPtr::from)?
            .local_addr()
            .map_err(ExceptionPtr::from)?
            .port();
        let mut lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        lo.set_fixed_cpu(this_shard_id());
        s.as_mut().listen_with(make_ipv4_address_ip_port(LOCALHOST, port), lo, None).get()?;

        let before = s.reply_errors();
        let peer = raw_exchange(port, b"GET /broken HTTP/1.1\r\nHost: t\r\n\r\n");
        wait_until("the peer to finish", || peer.is_finished())?;
        let reply = peer.join().expect("the peer thread").expect("the exchange");
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        wait_until("every connection to close", || s.current_connections() == 0)?;
        assert_eq!(s.reply_errors(), before + 1, "one failed reply, counted more than once");
        server_mut(&mut s).stop().get()?;
        Ok(())
    })
});
