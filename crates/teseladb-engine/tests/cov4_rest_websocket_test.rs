//! WebSocket frames past the handshake, from a raw client: messages with 16-bit and 64-bit
//! lengths (both ways: the client's frame and the server's echo), a close frame that ends the
//! connection, and a pong, which the parser refuses as an unknown opcode (as the original's
//! does), closing the connection.
#![cfg(all(target_os = "linux", feature = "tls"))]

#[path = "common/ws_raw_client.rs"]
mod ws_raw_client;

use teseladb::engine_thread_test_case;
use ws_raw_client::{client, frame, off_reactor, with_echo_server};

engine_thread_test_case!(long_messages_close_and_pong, {
    let failures = with_echo_server(|port| {
        let mut failures = Vec::new();
        for size in [300usize, 70_000] {
            if let Err(e) = off_reactor(move || client(port, &[], size)) {
                failures.push(format!("{size} bytes: {e}"));
            }
        }
        // A pong is refused as an unknown opcode and the connection closed, as in the
        // original, whose parser knows only the opcodes below 0xA (websocket/parser.hh,
        // is_opcode_known): the message after it is never echoed.
        let pong = off_reactor(move || client(port, &[frame(0xA, b"unsolicited")], 5));
        match pong {
            Err(e) if e.starts_with("frame header") => {}
            other => failures.push(format!("pong: expected the connection closed, got {other:?}")),
        }
        failures
    });
    assert!(failures.is_empty(), "{failures:?}");
});
