//! Defect: the payload of a ping stayed in the frame parser, and the next frame was copied
//! into it. The connection takes the payload of data frames only, so after a ping with a
//! 13-byte payload a 5-byte message was echoed as 13 bytes (the message over the start of the
//! ping's payload), and a 300-byte message was copied past the end of the 13-byte buffer
//! (websocket/parser.rs, the copy into `_result` of the payload state). The original
//! (websocket/parser.cc, `websocket_parser::operator()`) behaves the same: its echo demo
//! answers both messages with 13 bytes. A frame now starts with no payload, so every message
//! after a ping is echoed whole.
#![cfg(all(target_os = "linux", feature = "tls"))]

#[path = "common/ws_raw_client.rs"]
mod ws_raw_client;

use teseladb::engine_thread_test_case;
use ws_raw_client::{client, frame, off_reactor, with_echo_server};

engine_thread_test_case!(message_after_ping_is_echoed_whole, {
    let failures = with_echo_server(|port| {
        let mut failures = Vec::new();
        for size in [5usize, 13, 300, 70_000] {
            let ping = vec![frame(0x9, b"are you there")];
            if let Err(e) = off_reactor(move || client(port, &ping, size)) {
                failures.push(format!("{size} bytes after a ping: {e}"));
            }
        }
        failures
    });
    assert!(failures.is_empty(), "{failures:?}");
});
