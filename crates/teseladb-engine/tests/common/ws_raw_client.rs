//! A raw WebSocket client for the server tests: masked frames, the upgrade, an echo check,
//! and an echo server on this shard.

#![allow(dead_code)]
#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::loop_::{StopIteration, repeat};
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::net::api::{ListenOptions, LoadBalancingAlgorithm};
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::websocket::common::websocket::HandlerT;
use teseladb::engine::websocket::server::websocket::Server;

/// Echoes every message back.
pub fn echo_handler() -> HandlerT {
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

/// Runs `f` on another OS thread while this engine thread keeps the reactor serving.
pub fn off_reactor<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                sleep(Duration::from_millis(2)).get().expect("sleep");
            }
            Err(e) => panic!("the client did not finish: {e:?}"),
        }
    }
}

/// A masked client frame.
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut f = vec![0x80 | opcode];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 0xFFFF {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

/// Reads one server frame: its first byte and its payload.
pub fn read_frame(s: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    let e = |what: &'static str| move |err: std::io::Error| format!("{what}: {err}");
    let mut h = [0u8; 2];
    s.read_exact(&mut h).map_err(e("frame header"))?;
    if h[1] & 0x80 != 0 {
        return Err("a masked server frame".into());
    }
    let n = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            s.read_exact(&mut b).map_err(e("16-bit length"))?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            s.read_exact(&mut b).map_err(e("64-bit length"))?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut p = vec![0u8; n];
    s.read_exact(&mut p).map_err(e("frame payload"))?;
    Ok((h[0], p))
}

/// Skips the HTTP answer to the upgrade request; returns its status line.
pub fn read_upgrade_answer(s: &mut TcpStream) -> Result<String, String> {
    let mut got = Vec::new();
    let mut b = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).map_err(|e| format!("upgrade answer: {e}"))?;
        got.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&got).lines().next().unwrap_or_default().to_string())
}

/// Connects, upgrades, sends `before` (frames that get no answer), then a binary message of
/// `size` bytes, and checks its echo; ends with a close frame.
pub fn client(port: u16, before: &[Vec<u8>], size: usize) -> Result<(), String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let w = |s: &mut TcpStream, b: &[u8]| s.write_all(b).map_err(|e| format!("write: {e}"));
    w(
        &mut s,
        b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
          Sec-WebSocket-Protocol: echo\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Sec-WebSocket-Version: 13\r\n\r\n",
    )?;
    let status = read_upgrade_answer(&mut s)?;
    if !status.starts_with("HTTP/1.1 101") {
        return Err(status);
    }
    for f in before {
        w(&mut s, f)?;
    }
    let msg: Vec<u8> = (0..size as u32).map(|i| (i % 251) as u8).collect();
    w(&mut s, &frame(0x2, &msg))?;
    let mut echoed = Vec::new();
    while echoed.len() < msg.len() {
        let (h, p) = read_frame(&mut s)?;
        if h & 0x0F == 0xA {
            return Err("a pong was answered".into());
        }
        echoed.extend_from_slice(&p);
    }
    if echoed != msg {
        return Err(format!("echo of {} bytes differs", echoed.len()));
    }
    w(&mut s, &frame(0x8, &[0x03, 0xE8]))?;
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    Ok(())
}

/// Runs `f` with the port of a WebSocket server serving the echo subprotocol `echo` on this
/// shard, then stops the server (also when `f` fails).
pub fn with_echo_server<T>(f: impl FnOnce(u16) -> T) -> T {
    let mut server = Box::pin(Server::default());
    // SAFETY: the server is not moved out of its pin.
    unsafe { server.as_mut().get_unchecked_mut() }.register_handler("echo", echo_handler());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    let lo = ListenOptions {
        reuse_address: true,
        lba: LoadBalancingAlgorithm::Fixed,
        fixed_cpu: this_shard_id(),
        ..ListenOptions::default()
    };
    server.as_mut().listen_opts(SocketAddress::from_inet_address(&ip, port), lo).unwrap();
    let r = f(port);
    // SAFETY: as above.
    unsafe { server.as_mut().get_unchecked_mut() }.stop().get().unwrap();
    r
}
