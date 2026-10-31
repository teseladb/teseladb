//! Joins over the inter-node protocol: a node without an id sends `Hello` with `node_id = 0` and a
//! `JoinRequest`, and the acceptor answers with the cluster's decision on the same connection.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use teseladb_io::handshake::{
    Acceptor, HandshakeError, Inbound, JoinHandler, JoinRequest, request_join,
};
use teseladb_io::mailbox::{Notify, channel};
use teseladb_io::peer::{Hello, JoinAccepted, PeerMsg, decode_frame, encode_frame};
use teseladb_types::NodeId;

const CLUSTER: [u8; 16] = [7; 16];

struct Quiet;

impl Notify for Quiet {
    fn notify(&self) {}
}

fn hello(node: u64, addr: &str) -> Hello {
    Hello {
        cluster_id: CLUSTER,
        node_id: NodeId(node),
        core: 0,
        cores: 1,
        peer_addr: addr.into(),
        http_addr: String::new(),
        native_addr: String::new(),
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

/// An acceptor of node 1 with the given join handler; returns it and its address.
fn acceptor(join: Option<JoinHandler>) -> (Acceptor, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, _rx) = channel::<Inbound>(Arc::new(Quiet));
    let a = Acceptor::spawn_with_join(listener, hello(1, &addr), vec![tx], |i| i, join).unwrap();
    (a, addr)
}

fn frame(msg: &PeerMsg) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(msg, false, &mut out).unwrap();
    out
}

/// Reads frames from `s` until it is closed.
fn frames_until_closed(s: &mut TcpStream) -> Vec<PeerMsg> {
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while let Ok(Some((m, n))) = decode_frame(&buf[at..]) {
        out.push(m);
        at += n;
    }
    out
}

#[test]
fn an_accepted_join_returns_the_assigned_identity() {
    let accepted = JoinAccepted {
        node_id: NodeId(4),
        cluster_id: CLUSTER,
        cores: 2,
        voters: vec![(NodeId(1), [1; 32])],
        denied: vec![[9; 32]],
    };
    let answer = accepted.clone();
    let handler: JoinHandler = Arc::new(move |req| {
        assert_eq!(req, request());
        Ok(answer.clone())
    });
    let (a, addr) = acceptor(Some(handler));
    assert_eq!(request_join(&addr, &request()).unwrap(), Ok(accepted));
    a.stop();
}

#[test]
fn a_rejected_join_returns_the_reason() {
    let handler: JoinHandler = Arc::new(|_| Err("the cluster is full".into()));
    let (a, addr) = acceptor(Some(handler));
    assert_eq!(request_join(&addr, &request()).unwrap(), Err("the cluster is full".into()));
    a.stop();
}

#[test]
fn a_node_that_takes_no_joins_closes_the_connection() {
    let (a, addr) = acceptor(None);
    assert!(request_join(&addr, &request()).is_err());
    a.stop();
}

#[test]
fn a_joiner_that_sends_something_else_is_closed_without_an_answer() {
    let handler: JoinHandler = Arc::new(|_| panic!("no join request was sent"));
    let (a, addr) = acceptor(Some(handler));
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(&frame(&PeerMsg::Hello(hello(0, "")))).unwrap();
    s.write_all(&frame(&PeerMsg::Goodbye { reason: "x".into() })).unwrap();
    let got = frames_until_closed(&mut s);
    assert!(matches!(&got[..], [PeerMsg::Hello(h)] if h.node_id == NodeId(1)), "{got:?}");
    a.stop();
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
