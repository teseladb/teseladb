//! The cases of `engine_peers`, run in order in one engine of two shards.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use teseladb_io::engine::{CoreInput, EngineConfig, NodeEngine};
use teseladb_io::handshake::{
    self, AcceptorOptions, EnrollHandler, HandshakeError, JoinHandler, JoinRequest, request_enroll,
    request_join, request_join_with,
};
use teseladb_io::peer::{
    EnrollAccepted, EnrollRequest, Hello, JoinAccepted, PeerMsg, decode_frame, encode_frame,
};
use teseladb_io::secure::{Channel, PeerTls};
use teseladb_raftlog::MemDisk;
use teseladb_tls::{ClientConfig, Trust};
use teseladb_types::NodeId;

use crate::tls::{ca, ec, peer_tls};
use crate::toy::*;

type Case = fn(&NodeEngine);

pub fn run() {
    let engine = NodeEngine::start(&EngineConfig { shards: 2, ..EngineConfig::default() }).unwrap();
    let cases: &[(&str, Case)] = &[
        ("nodes_of_one_ca_talk_over_mtls", nodes_of_one_ca_talk_over_mtls),
        ("peers_without_the_cas_certificate_are_refused", peers_without_a_certificate_refused),
        ("a_node_enrolls_trusting_the_ca_by_its_fingerprint", a_node_enrolls),
        ("a_join_handler_does_not_take_an_enrollment", a_join_handler_takes_no_enrollment),
        ("replaced_settings_serve_the_next_connections", replaced_settings_serve_the_next),
        ("an_accepted_join_returns_the_assigned_identity", an_accepted_join),
        ("a_rejected_join_returns_the_reason", a_rejected_join),
        ("a_node_that_takes_no_joins_closes_the_connection", a_node_that_takes_no_joins),
        ("a_joiner_that_sends_something_else_is_closed", a_joiner_that_sends_something_else),
        ("each_connection_reaches_the_shard_of_its_core", each_connection_reaches_its_core),
        ("a_core_dials_from_a_port_of_its_own", a_core_dials_from_a_port_of_its_own),
        ("a_reserved_peer_port_is_taken_over_by_the_listener", a_reserved_peer_port_is_taken_over),
        ("a_peer_port_in_use_cannot_be_reserved", a_peer_port_in_use_cannot_be_reserved),
    ];
    // `CASES=a,b` runs only those.
    let only = std::env::var("CASES").ok();
    let wanted = |name: &str| only.as_deref().is_none_or(|o| o.split(',').any(|w| w == name));
    for (name, case) in cases.iter().filter(|(n, _)| wanted(n)) {
        case(&engine);
        println!("{name}: ok");
    }
    engine.stop();
}

fn tls_node(e: &NodeEngine, shard: u16, id: u64, toy: Toy, tls: &PeerTls) -> Node {
    start_with(e, shard, id, toy, MemDisk::new(), Duration::from_millis(5), Some(tls.clone()))
}

/// A blocking dial of `addr` as node `id`, over TLS with `client`.
fn dial(
    addr: SocketAddr,
    id: u64,
    client: Option<Arc<ClientConfig>>,
) -> Result<(TcpStream, Channel), HandshakeError> {
    let mut s = TcpStream::connect(addr)?;
    let mut ch = Channel::dialed(client.as_ref())?;
    handshake::dial_on(&mut s, &mut ch, &hello(id, "127.0.0.1:1"))?;
    Ok((s, ch))
}

fn frame(msg: &PeerMsg) -> Vec<u8> {
    let mut out = Vec::new();
    encode_frame(msg, false, &mut out).unwrap();
    out
}

fn nodes_of_one_ca_talk_over_mtls(e: &NodeEngine) {
    let the_ca = ca(1);
    let seen = Arc::default();
    let a = tls_node(e, 0, 1, Toy::default(), &peer_tls(&the_ca, 2));
    let b = tls_node(
        e,
        1,
        2,
        Toy { shared_seen: Arc::clone(&seen), ..Toy::default() },
        &peer_tls(&the_ca, 3),
    );
    send(&a, Cmd::Addr(NodeId(2), b.addr.to_string()));
    send(&b, Cmd::Addr(NodeId(1), a.addr.to_string()));
    send(&a, Cmd::Ping(NodeId(2), 100));
    // A stops answering at its limit: the last request B sees is the one before it.
    wait(|| load(&seen) >= 99);
    let (ta, _, _) = stop(e, a);
    let (tb, _, _) = stop(e, b);
    // A stopped first: its links never failed (B saw A go).
    assert!(ta.unreachable.is_empty());
    assert!(tb.last_seen >= 99);
    // The runtime does not send files itself, over TLS or not (023 FR-017).
    assert_eq!(ta.file_sends, Some(false));
}

fn peers_without_a_certificate_refused(e: &NodeEngine) {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let a = tls_node(e, 0, 1, Toy::default(), &tls);
    // A peer of the CA gets in, and its frames arrive.
    let (mut s, mut ch) = dial(a.addr, 5, Some(peer_tls(&the_ca, 5).client())).unwrap();
    assert!(!ch.peer_certificates().is_empty());
    ch.send(&mut s, &frame(&ack(7))).unwrap();
    // Without a client certificate, with another CA's, or in clear: refused.
    let mut bare = (*tls.client()).clone();
    bare.identity = None;
    assert!(dial(a.addr, 6, Some(Arc::new(bare))).is_err());
    assert!(dial(a.addr, 6, Some(peer_tls(&ca(9), 4).client())).is_err());
    assert!(dial(a.addr, 6, None).is_err());
    thread::sleep(Duration::from_millis(100));
    let (ta, _, _) = stop(e, a);
    assert_eq!(ta.last_seen, 7);
    assert!(ta.frames.iter().all(|(from, _)| *from == NodeId(5)));
    // A join needs a certificate too.
    let join: JoinHandler = Arc::new(|_| Err("no".into()));
    let b = acceptor_only(
        e,
        AcceptorOptions { join: Some(join), tls: Some(tls.clone()), enroll: None },
    );
    let req = join_request();
    let mut bare = (*tls.client()).clone();
    bare.identity = None;
    assert!(request_join_with(&b.0.to_string(), &req, Some(&Arc::new(bare))).is_err());
    assert_eq!(
        request_join_with(&b.0.to_string(), &req, Some(&peer_tls(&the_ca, 5).client())).unwrap(),
        Err("no".into())
    );
    stop_acceptor(e, b);
}

/// A node with no core of its own on this engine's shards to read its connections: a listener
/// for the newcomers' answers, on shard 1 with a core that drops what it gets.
fn acceptor_only(e: &NodeEngine, opts: AcceptorOptions) -> (SocketAddr, Node) {
    let addr = free_port();
    let h = hello(1, &addr.to_string());
    let core = start_core(e, 1, &h, Toy::default(), MemDisk::new(), Duration::from_millis(5), None);
    e.listen_peers(addr, &h, &opts, 1, 1).unwrap();
    (addr, Node { tx: core.sender(), addr, core })
}

fn stop_acceptor(e: &NodeEngine, a: (SocketAddr, Node)) {
    drop(stop(e, a.1));
}

fn join_request() -> JoinRequest {
    JoinRequest {
        name: "n4".into(),
        peer_addr: "127.0.0.1:1".into(),
        http_addr: "127.0.0.1:2".into(),
        native_addr: "127.0.0.1:3".into(),
        cores: 0,
    }
}

fn a_node_enrolls(e: &NodeEngine) {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let accepted = EnrollAccepted { certificate: vec![1], ca: vec![2], sealed: vec![3] };
    let answer = accepted.clone();
    let enroll: EnrollHandler = Arc::new(move |req| {
        assert_eq!(req.name, "n4");
        Ok(answer.clone())
    });
    let a = acceptor_only(e, AcceptorOptions { tls: Some(tls), enroll: Some(enroll), join: None });
    let req = EnrollRequest {
        token_id: [1; 16],
        secret: [2; 32],
        name: "n4".into(),
        public_key: vec![4; 91],
    };
    let pin = teseladb_crypto::sha256(the_ca.0.der());
    let pinned =
        |fp| Arc::new(ClientConfig::new(Trust { pinned: Some(fp), ..Trust::anchors(Vec::new()) }));
    let addr = a.0.to_string();
    assert_eq!(request_enroll(&addr, &req, &pinned(pin)).unwrap(), Ok(accepted));
    // Another fingerprint: the TLS handshake fails before anything is sent.
    assert!(matches!(request_enroll(&addr, &req, &pinned([0; 32])), Err(HandshakeError::Io(_))));
    stop_acceptor(e, a);
}

fn a_join_handler_takes_no_enrollment(e: &NodeEngine) {
    let join: JoinHandler = Arc::new(|_| {
        Ok(JoinAccepted {
            node_id: NodeId(4),
            cluster_id: CLUSTER,
            cores: 1,
            voters: Vec::new(),
            denied: Vec::new(),
        })
    });
    let a = acceptor_only(e, AcceptorOptions { join: Some(join), ..AcceptorOptions::default() });
    let req = EnrollRequest {
        token_id: [1; 16],
        secret: [2; 32],
        name: "n4".into(),
        public_key: Vec::new(),
    };
    let anyone = Arc::new(ClientConfig::new(Trust::anchors(Vec::new())));
    // In clear the acceptor gets no TLS handshake from an enrolling node.
    assert!(request_enroll(&a.0.to_string(), &req, &anyone).is_err());
    stop_acceptor(e, a);
}

fn replaced_settings_serve_the_next(e: &NodeEngine) {
    let the_ca = ca(1);
    let tls = peer_tls(&the_ca, 2);
    let a = tls_node(e, 0, 1, Toy::default(), &tls);
    let dialer = peer_tls(&the_ca, 3);
    // The leaf's key tells the identity apart.
    let leaf = |ch: &Channel| ch.peer_certificates()[0].public_key.clone();
    let (mut first, mut first_ch) = dial(a.addr, 5, Some(dialer.client())).unwrap();
    assert_eq!(leaf(&first_ch), ec(2).public_key());
    // The node rotates: shard 0 puts a copy of the new settings on every shard (023 FR-023).
    let (mine, next) = (tls.clone(), peer_tls(&the_ca, 6));
    e.run_on(0, move || {
        drop(mine.publish(&next));
    })
    .unwrap();
    // Each shard has its copy once shard 0's messages reached it.
    let until = std::time::Instant::now() + Duration::from_secs(10);
    for shard in 0..e.shards() {
        loop {
            let t = tls.clone();
            let fresh = e.run_on(shard, move || t.server()).unwrap();
            if fresh.identity.key.public_key() == ec(6).public_key() {
                break;
            }
            assert!(std::time::Instant::now() < until, "shard {shard} kept the old settings");
            thread::sleep(Duration::from_millis(5));
        }
    }
    // The test's thread is no shard: it keeps the settings it read.
    assert_eq!(tls.server().identity.key.public_key(), ec(2).public_key());
    let (_s, ch) = dial(a.addr, 6, Some(dialer.client())).unwrap();
    assert_eq!(leaf(&ch), ec(6).public_key());
    // The first connection still carries frames.
    first_ch.send(&mut first, &frame(&ack(9))).unwrap();
    thread::sleep(Duration::from_millis(100));
    let (ta, _, _) = stop(e, a);
    assert!(ta.frames.iter().any(|(from, m)| *from == NodeId(5) && *m == ack(9)));
}

fn an_accepted_join(e: &NodeEngine) {
    let accepted = JoinAccepted {
        node_id: NodeId(4),
        cluster_id: CLUSTER,
        cores: 2,
        voters: vec![(NodeId(1), [1; 32])],
        denied: vec![[9; 32]],
    };
    let answer = accepted.clone();
    let handler: JoinHandler = Arc::new(move |req| {
        assert_eq!(req, join_request());
        // The handler may wait for the cluster: in an engine thread, which parks meanwhile
        // (023 FR-026).
        assert!(teseladb_io::engine::wait::in_engine_thread());
        teseladb_io::engine::wait::sleep(Duration::from_millis(50));
        Ok(answer.clone())
    });
    let a = acceptor_only(e, AcceptorOptions { join: Some(handler), ..AcceptorOptions::default() });
    assert_eq!(request_join(&a.0.to_string(), &join_request()).unwrap(), Ok(accepted));
    stop_acceptor(e, a);
}

/// The peer port is reserved on shard 0 before the node's cores run (023 FR-015, T055): port 0
/// picks one, nobody else can take it, and the listener at that address takes the socket over.
fn a_reserved_peer_port_is_taken_over(e: &NodeEngine) {
    let at = e.reserve_peers("127.0.0.1:0".parse().unwrap(), 1, 1).unwrap();
    assert_eq!(at.ip().to_string(), "127.0.0.1");
    assert_ne!(at.port(), 0, "the address is known before the node listens");
    assert!(std::net::TcpListener::bind(at).is_err(), "the reservation holds the port");
    let h = hello(1, &at.to_string());
    let core = start_core(e, 1, &h, Toy::default(), MemDisk::new(), Duration::from_millis(5), None);
    let handler: JoinHandler = Arc::new(|_| Err("the cluster is full".into()));
    let opts = AcceptorOptions { join: Some(handler), ..AcceptorOptions::default() };
    assert_eq!(e.listen_peers(at, &h, &opts, 1, 1).unwrap(), at);
    let got = request_join(&at.to_string(), &join_request()).unwrap();
    assert_eq!(got, Err("the cluster is full".into()));
    stop_acceptor(e, (at, Node { tx: core.sender(), addr: at, core }));
}

/// A peer port another process holds fails the reservation with the system's reason.
fn a_peer_port_in_use_cannot_be_reserved(e: &NodeEngine) {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = held.local_addr().unwrap();
    let busy = e.reserve_peers(at, 1, 1).unwrap_err();
    assert_eq!(busy.kind(), std::io::ErrorKind::AddrInUse, "{busy}");
    drop(held);
}

fn a_rejected_join(e: &NodeEngine) {
    let handler: JoinHandler = Arc::new(|_| Err("the cluster is full".into()));
    let a = acceptor_only(e, AcceptorOptions { join: Some(handler), ..AcceptorOptions::default() });
    let got = request_join(&a.0.to_string(), &join_request()).unwrap();
    assert_eq!(got, Err("the cluster is full".into()));
    stop_acceptor(e, a);
}

fn a_node_that_takes_no_joins(e: &NodeEngine) {
    let a = acceptor_only(e, AcceptorOptions::default());
    assert!(request_join(&a.0.to_string(), &join_request()).is_err());
    stop_acceptor(e, a);
}

/// Reads frames from `s` until it is closed.
fn frames_until_closed(s: &mut TcpStream) -> Vec<PeerMsg> {
    let mut buf = Vec::new();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.read_to_end(&mut buf).unwrap();
    let mut out = Vec::new();
    let mut at = 0;
    while let Ok(Some((m, n))) = decode_frame(&buf[at..]) {
        out.push(m);
        at += n;
    }
    out
}

fn a_joiner_that_sends_something_else(e: &NodeEngine) {
    let handler: JoinHandler = Arc::new(|_| panic!("no join request was sent"));
    let a = acceptor_only(e, AcceptorOptions { join: Some(handler), ..AcceptorOptions::default() });
    let mut s = TcpStream::connect(a.0).unwrap();
    s.write_all(&frame(&PeerMsg::Hello(hello(0, "")))).unwrap();
    s.write_all(&frame(&PeerMsg::Goodbye { reason: "x".into() })).unwrap();
    let got = frames_until_closed(&mut s);
    assert!(matches!(&got[..], [PeerMsg::Hello(h)] if h.node_id == NodeId(1)), "{got:?}");
    stop_acceptor(e, a);
}

/// Node 40 with two cores, one on each shard, listening at the returned address.
fn two_cores(e: &NodeEngine) -> (SocketAddr, [Node; 2]) {
    let addr = free_port();
    let h = Hello { cores: 2, ..hello(40, &addr.to_string()) };
    let nodes = [0u16, 1].map(|k| {
        let hk = Hello { core: k, ..h.clone() };
        let core =
            start_core(e, k, &hk, Toy::default(), MemDisk::new(), Duration::from_millis(5), None);
        Node { tx: core.sender(), addr, core }
    });
    e.listen_peers(addr, &h, &AcceptorOptions::default(), 0, 2).unwrap();
    (addr, nodes)
}

fn each_connection_reaches_its_core(e: &NodeEngine) {
    let (addr, [c0, c1]) = two_cores(e);
    // Peers of two cores dial each of node 40's cores, from whatever port the kernel picks: the
    // engine hands each to the shard of its port (`Port`), and the core of the other parity gets
    // it through a relay. Each core gets exactly the frames sent to it.
    let mut streams = Vec::new();
    for (i, core) in [0u16, 1, 0, 1, 1, 0].into_iter().enumerate() {
        let mut s = TcpStream::connect(addr).unwrap();
        let h = Hello { cores: 2, core, ..hello(50 + i as u64, "127.0.0.1:1") };
        handshake::dial(&mut s, &h).unwrap();
        s.write_all(&frame(&ack(100 + i as u64))).unwrap();
        streams.push((s, core));
    }
    thread::sleep(Duration::from_millis(200));
    e.stop_peers(Some(addr));
    let stopped: Vec<Toy> = [c0, c1]
        .into_iter()
        .map(|n| {
            n.tx.send(CoreInput::Stop).ok().unwrap();
            n.core.join().unwrap().0
        })
        .collect();
    for (k, t) in stopped.iter().enumerate() {
        let want: Vec<u64> = streams
            .iter()
            .enumerate()
            .filter(|(_, (_, core))| usize::from(*core) == k)
            .map(|(i, _)| 100 + i as u64)
            .collect();
        let got: Vec<u64> = t
            .frames
            .iter()
            .map(|(_, m)| match m {
                PeerMsg::ForwardResp { req, .. } => *req,
                other => panic!("{other:?}"),
            })
            .collect();
        let mut got_sorted = got.clone();
        got_sorted.sort_unstable();
        assert_eq!(got_sorted, want, "core {k}");
    }
    // A stopped core closed its connections, the relayed ones too.
    for (mut s, _) in streams {
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut b = [0u8; 16];
        assert!(matches!(s.read(&mut b), Ok(0) | Err(_)));
    }
}

fn a_core_dials_from_a_port_of_its_own(e: &NodeEngine) {
    let (addr, [c0, c1]) = two_cores(e);
    // A fake peer of two cores: the connection of node 40's core 1 comes from an odd port.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = l.local_addr().unwrap().to_string();
    let h = Hello { cores: 2, ..hello(41, &peer) };
    let fake = thread::spawn(move || {
        let (mut s, from) = l.accept().unwrap();
        let got = handshake::answer(&mut s, &Hello { core: 1, ..h }).unwrap();
        (from.port(), got.hello.core)
    });
    send(&c1, Cmd::Addr(NodeId(41), peer));
    send(&c1, Cmd::Bulk(NodeId(41), 1, 10));
    let (port, core) = fake.join().unwrap();
    assert_eq!((port % 2, core), (1, 1));
    e.stop_peers(Some(addr));
    for n in [c0, c1] {
        n.tx.send(CoreInput::Stop).ok().unwrap();
        drop(n.core.join());
    }
}
