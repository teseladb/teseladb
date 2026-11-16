//! The joining side of the inter-node protocol: a node without an id sends `Hello` with
//! `node_id = 0` and a `JoinRequest`, and waits for the cluster's decision on the same connection.
//! The acceptor's side runs on the engine (`engine_peers`).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use teseladb_io::handshake::{HandshakeError, JoinRequest, request_join};
use teseladb_io::peer::{Hello, PeerMsg, Versions, decode_frame, encode_frame};
use teseladb_types::NodeId;

const CLUSTER: [u8; 16] = [7; 16];

fn hello(node: u64, addr: &str) -> Hello {
    Hello {
        cluster_id: CLUSTER,
        node_id: NodeId(node),
        core: 0,
        cores: 1,
        peer_addr: addr.into(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: Versions::OWN,
    }
}

fn request() -> JoinRequest {
    JoinRequest {
        name: "n4".into(),
        peer_addr: "127.0.0.1:1".into(),
        http_addr: "127.0.0.1:2".into(),
        native_addr: "127.0.0.1:3".into(),
        cores: 0,
    }
}

fn frame(msg: &PeerMsg) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(msg, false, &mut out).unwrap();
    out
}

#[test]
fn a_join_answered_with_another_frame_is_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let local = hello(1, &addr);
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        // The joiner's Hello and JoinRequest, then an answer that is not a JoinResponse.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut seen = 0;
        while seen < 2 {
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&chunk[..n]);
            seen = 0;
            let mut at = 0;
            while let Ok(Some((_, used))) = decode_frame(&buf[at..]) {
                seen += 1;
                at += used;
            }
            if seen == 1 {
                s.write_all(&frame(&PeerMsg::Hello(local.clone()))).unwrap();
            }
        }
        s.write_all(&frame(&PeerMsg::Goodbye { reason: "no".into() })).unwrap();
    });
    let r = request_join(&addr, &request());
    assert!(matches!(r, Err(HandshakeError::UnexpectedFrame)), "{r:?}");
    server.join().unwrap();
}

#[test]
fn an_address_that_does_not_resolve_is_an_error() {
    assert!(matches!(
        request_join("no-such-host.invalid:1", &request()),
        Err(HandshakeError::Io(_))
    ));
}
