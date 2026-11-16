#![cfg(target_os = "linux")]
//! Tests of the WebSocket parser and server beyond the handshake tests: frame header
//! fields, 16- and 64-bit payload lengths, rejected frames (unmasked, reserved bits, unknown
//! opcodes, impossible lengths), the error type and the hash helpers, and a server on a TCP
//! port that echoes long messages, ignores pings and pongs, answers a close frame, and copes
//! with a client that leaves before its upgrade request.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::loop_::{StopIteration, repeat};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::websocket::common::websocket::{
    Exception, HandlerT, encode_base64, sha1_base64,
};
use teseladb::engine::websocket::parser::websocket::{FrameHeader, Opcodes, WebsocketParser};
use teseladb::engine::websocket::server::websocket::Server;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

/// A client frame (masked with a zero key) carrying `payload`, with the length field in
/// the 7-, 16- or 64-bit form as the size requires.
fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | opcode];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= usize::from(u16::MAX) {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.extend_from_slice(payload);
    f
}

/// Feeds `bytes` to a fresh parser in one buffer and returns it with the outcome.
fn parse(bytes: &[u8]) -> (WebsocketParser, Result<(), ExceptionPtr>) {
    let mut p = WebsocketParser::default();
    let r = p.call(TemporaryBuffer::copy_from(bytes)).get().map(|_| ());
    (p, r)
}

#[test]
fn frame_headers_expose_their_fields() {
    let h = FrameHeader::new(&[0x81, 0x85]);
    assert_eq!(h.get_fin(), 1);
    assert_eq!(h.get_rsv1(), 0);
    assert_eq!(h.get_rsv2(), 0);
    assert_eq!(h.get_rsv3(), 0);
    assert_eq!(h.get_opcode(), 1);
    assert_eq!(h.get_masked(), 1);
    assert_eq!(h.get_length(), 5);
    assert!(h.is_opcode_known());
    let h = FrameHeader::new(&[0x7B, 0x7E]);
    assert_eq!(h.get_fin(), 0);
    assert_eq!((h.get_rsv1(), h.get_rsv2(), h.get_rsv3()), (1, 1, 1));
    assert_eq!(h.get_opcode(), 0xB);
    assert!(!h.is_opcode_known());
    assert_eq!(h.get_masked(), 0);
    assert_eq!(h.get_length(), 126);
    assert_eq!(h.get_rest_of_header_length(), 2 + 4);
    assert_eq!(FrameHeader::new(&[0x82, 0xFF]).get_rest_of_header_length(), 8 + 4);
}

#[test]
fn long_payloads_use_extended_lengths() {
    assert_eq!(WebsocketParser::default().opcode(), Opcodes::INVALID);

    let payload: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
    let (mut p, r) = parse(&client_frame(0x2, &payload));
    assert!(r.is_ok());
    assert!(p.is_valid());
    assert_eq!(p.opcode(), Opcodes::BINARY);
    assert_eq!(p.result().as_slice(), &payload[..]);

    let payload = vec![7u8; 70_000];
    let (mut p, r) = parse(&client_frame(0x1, &payload));
    assert!(r.is_ok());
    assert_eq!(p.opcode(), Opcodes::TEXT);
    assert_eq!(p.result().size(), 70_000);
}

#[test]
fn invalid_frames_stop_the_parser() {
    // Unmasked, a reserved bit, an unknown opcode.
    for bytes in [&[0x82u8, 0x05][..], &[0xC2, 0x85], &[0x83, 0x85]] {
        let (p, r) = parse(bytes);
        assert!(r.is_ok());
        assert!(!p.is_valid());
        assert!(!p.eof());
    }
    // A length that cannot be allocated fails the consume.
    let mut bytes = vec![0x82, 0xFF];
    bytes.extend_from_slice(&u64::MAX.to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0, 1]);
    let (_, r) = parse(&bytes);
    assert!(r.is_err());
    // An empty buffer is the end of the stream.
    let (p, r) = parse(&[]);
    assert!(r.is_ok());
    assert!(p.eof());
}

#[test]
fn errors_and_hash_helpers() {
    let e = Exception::new("bad frame");
    assert_eq!(e.what(), "bad frame");
    assert_eq!(e.to_string(), "bad frame");
    assert!(encode_base64(b"hello").unwrap().starts_with(b"aGVsbG8="));
    let accept =
        sha1_base64(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11").unwrap();
    assert!(accept.starts_with(b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
}

/// Echoes every message back.
fn echo_handler() -> HandlerT {
    Rc::new(|in_: &mut InputStream<u8>, out: &mut OutputStream<u8>| -> Future<()> {
        let inp: *mut InputStream<u8> = in_;
        let outp: *mut OutputStream<u8> = out;
        repeat(move || {
            // SAFETY: the connection keeps both streams alive while the handler runs.
            unsafe { (*inp).read() }.then(move |f: TemporaryBuffer<u8>| -> Future<StopIteration> {
                if f.empty() {
                    return make_ready_future(StopIteration::YES);
                }
                // SAFETY: as above.
                unsafe { (*outp).write_buffer(f) }.then(move |()| {
                    // SAFETY: as above.
                    unsafe { (*outp).flush() }.then(|()| make_ready_future(StopIteration::NO))
                })
            })
        })
    })
}

/// What the client saw: the echoed payload size, the length forms of the server's frames,
/// and whether the server answered the close frame.
#[derive(Debug, Default)]
struct Seen {
    echoed: usize,
    forms: Vec<u8>,
    closed: bool,
}

fn read_exact_or_none(s: &mut TcpStream, n: usize) -> Option<Vec<u8>> {
    let mut b = vec![0u8; n];
    s.read_exact(&mut b).ok()?;
    Some(b)
}

/// Reads one server frame: (opcode, length form, payload length).
fn read_frame(s: &mut TcpStream) -> Option<(u8, u8, usize)> {
    let h = read_exact_or_none(s, 2)?;
    let form = h[1] & 0x7F;
    let len = match form {
        126 => u16::from_be_bytes(read_exact_or_none(s, 2)?.try_into().unwrap()) as usize,
        127 => u64::from_be_bytes(read_exact_or_none(s, 8)?.try_into().unwrap()) as usize,
        n => usize::from(n),
    };
    read_exact_or_none(s, len)?;
    Some((h[0] & 0x0F, form, len))
}

fn websocket_client(port: u16) -> Seen {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.extend(read_exact_or_none(&mut s, 1).expect("upgrade response"));
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));

    s.write_all(&client_frame(0x9, b"")).unwrap();
    s.write_all(&client_frame(0xA, b"")).unwrap();
    s.write_all(&client_frame(0x2, &[1u8; 200])).unwrap();
    s.write_all(&client_frame(0x2, &[2u8; 70_000])).unwrap();
    let mut seen = Seen::default();
    while seen.echoed < 70_200 {
        let Some((_, form, len)) = read_frame(&mut s) else { break };
        seen.echoed += len;
        seen.forms.push(form);
    }
    s.write_all(&client_frame(0x8, b"")).unwrap();
    while let Some((opcode, _, _)) = read_frame(&mut s) {
        if opcode == Opcodes::CLOSE.0 {
            seen.closed = true;
            break;
        }
    }
    seen
}

/// Runs `f` on another OS thread while this engine thread keeps the reactor serving.
fn off_reactor<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                sleep(Duration::from_millis(2)).get().unwrap();
            }
            Err(e) => panic!("the client did not finish: {e:?}"),
        }
    }
}

thread_case!(a_tcp_server_echoes_long_messages_and_closes, {
    let mut server = Box::pin(Server::default());
    // SAFETY: the server is not moved out of its pin.
    unsafe { server.as_mut().get_unchecked_mut() }.register_handler("", echo_handler());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    server.as_mut().listen(SocketAddress::from_inet_address(&ip, port)).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A client that leaves before sending its upgrade request.
        off_reactor(move || drop(TcpStream::connect(("127.0.0.1", port)).unwrap()));
        let seen = off_reactor(move || websocket_client(port));
        assert_eq!(seen.echoed, 70_200, "{seen:?}");
        // The 200-byte echo needs the 16-bit length; the long one, unless the handler's
        // stream splits it, the 64-bit length.
        assert!(seen.forms.contains(&126), "{seen:?}");
        assert!(seen.closed, "{seen:?}");
    }));
    // SAFETY: as above.
    unsafe { server.as_mut().get_unchecked_mut() }.stop().get().unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
});
