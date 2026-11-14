//! TLS between nodes (013 T018, FR-006) and enrollment over it (FR-005): two nodes with
//! certificates of the cluster's CA handshake with mTLS and exchange frames; a peer without a
//! certificate, or with one of another CA, is refused; a node that enrolls needs no certificate
//! and trusts the CA by its fingerprint alone.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb_io::handshake::{
    Acceptor, AcceptorOptions, EnrollHandler, HandshakeError, Inbound, JoinHandler, JoinRequest,
    Outbound, connect_with, request_enroll, request_join_with,
};
use teseladb_io::mailbox::{Notify, channel};
use teseladb_io::peer::{EnrollAccepted, EnrollRequest, Hello, JoinAccepted, PeerMsg};
use teseladb_io::secure::Channel;
use teseladb_tls::{ClientConfig, Trust};
use teseladb_types::NodeId;

const CLUSTER: [u8; 16] = [7; 16];
struct Quiet;

impl Notify for Quiet {
    fn notify(&self) {}
}

#[path = "common/tls.rs"]
mod tls;
use tls::{ca, ec, peer_tls};

fn hello(node: u64) -> Hello {
    Hello {
        cluster_id: CLUSTER,
        node_id: NodeId(node),
        core: 0,
        cores: 1,
        peer_addr: "127.0.0.1:1".into(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: teseladb_io::peer::Versions::OWN,
    }
}

fn wait<T: Send>(rx: &mut teseladb_io::mailbox::Mailbox<T>) -> T {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let mut got = Vec::new();
        rx.drain_into(&mut got);
        if let Some(x) = got.pop() {
            return x;
        }
        assert!(Instant::now() < until, "nothing arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Node 1's acceptor with `opts`; its address and where its connections arrive.
fn acceptor(opts: AcceptorOptions) -> (Acceptor, String, teseladb_io::mailbox::Mailbox<Inbound>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = channel::<Inbound>(Arc::new(Quiet));
    let a = Acceptor::spawn_with(listener, hello(1), vec![tx], |i| i, opts).unwrap();
    (a, addr, rx)
}

fn dial(addr: &str, client: Option<Arc<ClientConfig>>) -> Outbound {
    let (tx, mut rx) = channel::<Outbound>(Arc::new(Quiet));
    connect_with(NodeId(1), addr.to_owned(), hello(2), client, tx, |o| o);
    wait(&mut rx)
}

/// A channel in clear passes the bytes as they are, and says so in its debug form.
#[test]
fn a_plain_channel_reads_until_the_peer_closes() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut a = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (mut b, _) = l.accept().unwrap();
    let mut ch = Channel::Plain;
    assert_eq!(format!("{ch:?}"), "Plain");
    ch.send(&mut a, b"abc").unwrap();
    let mut plain = Vec::new();
    ch.recv(&mut b, &mut plain).unwrap();
    assert_eq!(plain, b"abc");
    drop(a);
    let e = ch.recv(&mut b, &mut plain).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn nodes_of_one_ca_talk_over_mtls_and_others_are_refused() {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let opts = AcceptorOptions { tls: Some(tls.clone()), ..AcceptorOptions::default() };
    let (a, addr, mut inbound) = acceptor(opts);
    let out = dial(&addr, Some(peer_tls(&the_ca, 3).client()));
    let (mut stream, mut ch, got) = out.result.unwrap();
    assert_eq!(got.hello.node_id, NodeId(1));
    assert!(matches!(ch, Channel::Tls(_)));
    assert!(!ch.peer_certificates().is_empty());
    let mut accepted = wait(&mut inbound);
    assert_eq!(accepted.got.hello.node_id, NodeId(2));
    assert!(!accepted.channel.peer_certificates().is_empty());
    // A frame each way through the channels.
    let mut frame = Vec::new();
    teseladb_io::peer::encode_frame(&PeerMsg::Goodbye { reason: "x".into() }, false, &mut frame)
        .unwrap();
    ch.send(&mut stream, &frame).unwrap();
    let mut plain = Vec::new();
    accepted.channel.recv(&mut accepted.stream, &mut plain).unwrap();
    assert_eq!(plain, frame);
    // The debug forms leave the keys out.
    assert_eq!((format!("{tls:?}"), format!("{ch:?}")), ("PeerTls".into(), "Tls".into()));
    // Bytes that are not TLS end the read, and the channel answers them with an alert.
    std::io::Write::write_all(&mut stream, &[0x17, 0x03, 0x03, 0x00, 0x05, 1, 2, 3, 4, 5]).unwrap();
    let mut rest = Vec::new();
    let e = accepted.channel.recv(&mut accepted.stream, &mut rest).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    drop(stream);
    // Without a client certificate, with another CA's, or in clear: refused.
    let mut bare = (*tls.client()).clone();
    bare.identity = None;
    assert!(dial(&addr, Some(Arc::new(bare))).result.is_err());
    assert!(dial(&addr, Some(peer_tls(&ca(9), 4).client())).result.is_err());
    assert!(dial(&addr, None).result.is_err());
    // A join needs a certificate too.
    let join: JoinHandler = Arc::new(|_| Err("no".into()));
    let opts = AcceptorOptions { join: Some(join), tls: Some(tls.clone()), enroll: None };
    let (b, addr2, _rx) = acceptor(opts);
    let req = JoinRequest {
        name: "n".into(),
        peer_addr: String::new(),
        http_addr: String::new(),
        native_addr: String::new(),
        cores: 0,
    };
    let mut bare = (*tls.client()).clone();
    bare.identity = None;
    assert!(request_join_with(&addr2, &req, Some(&Arc::new(bare))).is_err());
    assert_eq!(
        request_join_with(&addr2, &req, Some(&peer_tls(&the_ca, 5).client())).unwrap(),
        Err("no".into())
    );
    a.stop();
    b.stop();
}

#[test]
fn a_node_enrolls_without_a_certificate_trusting_the_ca_by_its_fingerprint() {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let accepted = EnrollAccepted { certificate: vec![1], ca: vec![2], sealed: vec![3] };
    let answer = accepted.clone();
    let enroll: EnrollHandler = Arc::new(move |req| {
        assert_eq!(req.name, "n4");
        Ok(answer.clone())
    });
    let opts = AcceptorOptions { tls: Some(tls), enroll: Some(enroll), join: None };
    let (a, addr, _rx) = acceptor(opts);
    let req = EnrollRequest {
        token_id: [1; 16],
        secret: [2; 32],
        name: "n4".into(),
        public_key: vec![4; 91],
    };
    let pin = teseladb_crypto::sha256(the_ca.0.der());
    let pinned =
        |fp| Arc::new(ClientConfig::new(Trust { pinned: Some(fp), ..Trust::anchors(Vec::new()) }));
    assert_eq!(request_enroll(&addr, &req, &pinned(pin)).unwrap(), Ok(accepted));
    // Another fingerprint: the TLS handshake fails before anything is sent.
    assert!(matches!(request_enroll(&addr, &req, &pinned([0; 32])), Err(HandshakeError::Io(_))));
    assert!(format!("{req:?}").contains("n4") && !format!("{req:?}").contains("secret"));
    a.stop();
}

#[test]
fn a_join_handler_does_not_take_an_enrollment_nor_the_other_way() {
    let join: JoinHandler = Arc::new(|_| {
        Ok(JoinAccepted {
            node_id: NodeId(4),
            cluster_id: CLUSTER,
            cores: 1,
            voters: Vec::new(),
            denied: Vec::new(),
        })
    });
    let (a, addr, _rx) =
        acceptor(AcceptorOptions { join: Some(join), ..AcceptorOptions::default() });
    let req = EnrollRequest {
        token_id: [1; 16],
        secret: [2; 32],
        name: "n4".into(),
        public_key: Vec::new(),
    };
    let anyone = Arc::new(ClientConfig::new(Trust::anchors(Vec::new())));
    // In clear the acceptor gets no TLS handshake from an enrolling node.
    assert!(request_enroll(&addr, &req, &anyone).is_err());
    assert!(format!("{:?}", AcceptorOptions::default()).contains("tls: false"));
    a.stop();
}

#[test]
fn replaced_settings_serve_the_next_connections_and_open_ones_keep_theirs() {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let opts = AcceptorOptions { tls: Some(tls.clone()), ..AcceptorOptions::default() };
    let (a, addr, mut inbound) = acceptor(opts);
    let dialer = peer_tls(&the_ca, 3);
    // The leaf's key tells the identity apart (its validity starts at the current second).
    let leaf = |ch: &Channel| ch.peer_certificates()[0].public_key.clone();
    let (mut first, mut first_ch, _) = dial(&addr, Some(dialer.client())).result.unwrap();
    let mut first_in = wait(&mut inbound);
    assert_eq!(leaf(&first_ch), ec(2).public_key());
    assert_eq!(leaf(&first_in.channel), ec(3).public_key());
    // Both nodes rotate: a clone of the settings is enough.
    tls.clone().replace(&peer_tls(&the_ca, 6));
    dialer.replace(&peer_tls(&the_ca, 7));
    let (_s, ch, _) = dial(&addr, Some(dialer.client())).result.unwrap();
    let second_in = wait(&mut inbound);
    assert_eq!(leaf(&ch), ec(6).public_key());
    assert_eq!(leaf(&second_in.channel), ec(7).public_key());
    // The first connection still carries frames.
    let mut frame = Vec::new();
    teseladb_io::peer::encode_frame(&PeerMsg::Goodbye { reason: "y".into() }, false, &mut frame)
        .unwrap();
    first_ch.send(&mut first, &frame).unwrap();
    let mut plain = Vec::new();
    first_in.channel.recv(&mut first_in.stream, &mut plain).unwrap();
    assert_eq!(plain, frame);
    a.stop();
}
