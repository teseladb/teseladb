//! Tests of the WebSocket server: the opening handshake (with and without a subprotocol),
//! echo through a registered handler, and frame parsing.

#![cfg(all(target_os = "linux", feature = "tls"))]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/loopback_socket.rs"]
mod loopback_socket;

use std::rc::Rc;

use loopback_socket::{LoopbackConnectionFactory, LoopbackSocketImpl};
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{
    DataSource, DataSourceImpl, InputStream, ObsoleteConsumer, OutputStream,
};
use teseladb::engine::core::loop_::{StopIteration, repeat};
use teseladb::engine::core::sstring::{Sstring, to_sstring_buffer};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::response_parser::HttpResponseParser;
use teseladb::engine::net::socket_defs::{SocketAddress, Transport};
use teseladb::engine::net::stack::SocketImpl;
use teseladb::engine::util::defer::defer;
use teseladb::engine::util::string_utils::internal::CaseInsensitiveStr;
use teseladb::engine::websocket::common::websocket::HandlerT;
use teseladb::engine::websocket::parser::websocket::WebsocketParser;
use teseladb::engine::websocket::server::websocket::{Server, ServerConnection};

fn build_request(key_base64: &str, subprotocol: &str) -> String {
    let mut subprotocol_line = String::new();
    if !subprotocol.is_empty() {
        subprotocol_line = format!("Sec-WebSocket-Protocol: {}\r\n", subprotocol);
    }
    format!(
        "GET / HTTP/1.1\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {}\
         \r\n",
        key_base64, subprotocol_line
    )
}

/// The echo handler both tests register.
fn echo_handler() -> HandlerT {
    Rc::new(|in_: &mut InputStream<u8>, out: &mut OutputStream<u8>| -> Future<()> {
        let inp: *mut InputStream<u8> = in_;
        let outp: *mut OutputStream<u8> = out;
        repeat(move || {
            // SAFETY: the connection keeps both streams alive while the handler runs.
            unsafe { (*inp).read() }.then(move |f: TemporaryBuffer<u8>| -> Future<StopIteration> {
                eprintln!("f.size(): {}", f.size());
                if f.empty() {
                    make_ready_future(StopIteration::YES)
                } else {
                    // SAFETY: as above.
                    unsafe { (*outp).write_buffer(f) }.then(move |()| {
                        // SAFETY: as above.
                        unsafe { (*outp).flush() }.then(|()| make_ready_future(StopIteration::NO))
                    })
                }
            })
        })
    })
}

fn test_websocket_handshake_common(subprotocol: String) -> Future<()> {
    async_(move || -> Result<(), ExceptionPtr> {
        let request = build_request("dGhlIHNhbXBsZSBub25jZQ==", &subprotocol);

        let mut factory = LoopbackConnectionFactory::new_default();
        let mut lsi = LoopbackSocketImpl::new(&mut factory, None);

        let acceptor = factory.get_server_socket().accept();
        let connector =
            lsi.connect(SocketAddress::default(), SocketAddress::default(), Transport::TCP);
        let mut sock = connector.get()?;
        let mut input = sock.input(Default::default());
        let mut output = sock.output(8192);

        let mut dummy = Server::default();
        dummy.register_handler(&subprotocol, echo_handler());
        // SAFETY: `dummy` outlives the connection, which `_close` closes and waits for.
        let conn = unsafe { ServerConnection::new(&mut dummy, acceptor.get()?.connection) };
        let connp: *mut ServerConnection = conn.as_ptr();
        // SAFETY: `conn` owns the connection at `connp` until the end of the scope.
        let serve = unsafe { (*connp).process() };
        let inputp: *mut InputStream<u8> = &mut input;
        let outputp: *mut OutputStream<u8> = &mut output;
        let mut serve = Some(serve);
        let servep: *mut Option<Future<()>> = &mut serve;
        let _close = defer(move || {
            // SAFETY: all four locals are declared before the guard and outlive it.
            unsafe {
                let _ = (*connp).close(true).get();
                let _ = (*inputp).close().get();
                let _ = (*outputp).close().get();
                if let Some(s) = (*servep).take() {
                    let _ = s.get();
                }
            }
        });

        // Send the handshake
        output.write_str(&request).get()?;
        output.flush().get()?;

        // Check that the server correctly computed the response
        // according to WebSocket handshake specification
        let mut parser = HttpResponseParser::default();
        parser.init();
        let pp: *mut HttpResponseParser = &mut parser;
        // SAFETY: `parser` outlives the consume() it is used by.
        input.consume(ObsoleteConsumer(move |buf| unsafe { (*pp).call(buf) })).get()?;
        let resp = parser.get_parsed_response();
        assert!(resp.is_some());
        let Some(resp) = resp else { unreachable!() };
        let mut websocket_accept: Sstring = resp
            ._headers
            .get(CaseInsensitiveStr::new("Sec-WebSocket-Accept"))
            .cloned()
            .unwrap_or_default();
        // Trim possible whitespace prefix
        if let Some(it) = websocket_accept.as_bytes().iter().position(|c| c.is_ascii_alphanumeric())
        {
            websocket_accept = websocket_accept.substr(it, websocket_accept.size() - it)?;
        }
        assert_eq!(websocket_accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        for (k, v) in resp._headers.iter() {
            println!("{}:{}", k.0, v);
        }
        Ok(())
    })
}

teseladb::engine_test_case!(test_websocket_handshake, {
    test_websocket_handshake_common("echo".to_string())
});

teseladb::engine_test_case!(test_websocket_handshake_no_subprotocol, {
    test_websocket_handshake_common(String::new())
});

fn test_websocket_handler_registration_common(subprotocol: String) -> Future<()> {
    async_(move || -> Result<(), ExceptionPtr> {
        let mut factory = LoopbackConnectionFactory::new_default();
        let mut lsi = LoopbackSocketImpl::new(&mut factory, None);

        let acceptor = factory.get_server_socket().accept();
        let connector =
            lsi.connect(SocketAddress::default(), SocketAddress::default(), Transport::TCP);
        let mut sock = connector.get()?;
        let mut input = sock.input(Default::default());
        let mut output = sock.output(8192);

        // Setup server
        let mut ws = Server::default();
        ws.register_handler(&subprotocol, echo_handler());
        // SAFETY: `ws` outlives the connection, which `_close` closes and waits for.
        let conn = unsafe { ServerConnection::new(&mut ws, acceptor.get()?.connection) };
        let connp: *mut ServerConnection = conn.as_ptr();
        // SAFETY: `conn` owns the connection at `connp` until the end of the scope.
        let serve = unsafe { (*connp).process() };
        let inputp: *mut InputStream<u8> = &mut input;
        let outputp: *mut OutputStream<u8> = &mut output;
        let mut serve = Some(serve);
        let servep: *mut Option<Future<()>> = &mut serve;
        let _close = defer(move || {
            // SAFETY: all four locals are declared before the guard and outlive it.
            unsafe {
                let _ = (*connp).close(true).get();
                let _ = (*inputp).close().get();
                let _ = (*outputp).close().get();
                if let Some(s) = (*servep).take() {
                    let _ = s.get();
                }
            }
        });

        // handshake
        let request = build_request("dGhlIHNhbXBsZSBub25jZQ==", &subprotocol);
        output.write_str(&request).get()?;
        output.flush().get()?;

        let mut reply_size: usize = 156;
        if !subprotocol.is_empty() {
            reply_size += "\r\nSec-WebSocket-Protocol: ".len() + subprotocol.len();
        }
        input.read_exactly(reply_size).get()?;

        let ws_frame_len = 10usize;
        // Sending and receiving a websocket frame
        let ws_frame: &[u8] = b"\x82\x84\
                                TEST\
                                \0\0\0\0"; // 1000 0002 1000 0100, Masking Key, Masked Message - TEST
        assert_eq!(ws_frame.len(), ws_frame_len);
        let rs_frame: &[u8] = b"\x82\x04TEST"; // 1000 0002 0000 0100, Message - TEST
        output.write(ws_frame).get()?;
        output.flush().get()?;

        let response = input.read_exactly(6).get()?;
        assert_eq!(rs_frame, response.as_slice());
        Ok(())
    })
}

teseladb::engine_test_case!(test_websocket_handler_registration, {
    test_websocket_handler_registration_common("echo".to_string())
});

teseladb::engine_test_case!(test_websocket_handler_registration_no_subprotocol, {
    test_websocket_handler_registration_common(String::new())
});

// Simple wrapper to help create a testable input_stream.
#[derive(Default)]
struct TestSourceImpl {
    bufs: Vec<TemporaryBuffer<u8>>,
    idx: usize,
}

impl TestSourceImpl {
    fn push_back(&mut self, s: &[u8]) {
        let buf = TemporaryBuffer::copy_from(s);
        self.bufs.push(buf);
    }
}

impl DataSourceImpl for TestSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        if self.idx < self.bufs.len() {
            let b = self.bufs[self.idx].share();
            self.idx += 1;
            return make_ready_future(b);
        }
        make_ready_future(TemporaryBuffer::default())
    }
}

teseladb::engine_test_case!(test_websocket_parser_split, {
    async_(|| -> Result<(), ExceptionPtr> {
        // Two websocket frames
        let ws_frames: &[u8] = b"\x82\x85\
                                     \0\0\0\0\
                                     TEST1\
                                     \x82\x85\
                                     \0\0\0\0\
                                     TEST2\
                                     \x82\x85\
                                     \0\0\0\0\
                                     TEST3"; // FIN, opcode, mask, payload len (5); Masking Key; payload
        assert_eq!(ws_frames.len(), 33);

        for split_i in 0..ws_frames.len() - 1 {
            let mut parser = WebsocketParser::default();
            let mut source = Box::new(TestSourceImpl::default());
            if split_i == 0 {
                source.push_back(ws_frames);
            } else {
                source.push_back(&ws_frames[..split_i]);
                source.push_back(&ws_frames[split_i..]);
            }
            let mut in_ = InputStream::new(DataSource::new(source));
            let mut results: Vec<Sstring> = Vec::new();

            loop {
                let pp: *mut WebsocketParser = &mut parser;
                // SAFETY: `parser` outlives the consume() it is used by.
                in_.consume(move |buf| unsafe { (*pp).call(buf) }).get()?;
                if parser.eof() {
                    break;
                }
                assert!(parser.is_valid());
                results.push(to_sstring_buffer(&parser.result()));
            }
            assert!(!parser.is_valid());
            assert_eq!(0, parser.result().size());
            let expected: Vec<Sstring> = vec!["TEST1".into(), "TEST2".into(), "TEST3".into()];
            assert_eq!(results, expected);
        }
        Ok(())
    })
});
