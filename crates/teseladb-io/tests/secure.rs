//! The blocking side of a peer connection's channel (013 FR-006): in clear it passes the bytes as
//! they are. TLS between nodes, enrollments and their refusals run on the engine
//! (`engine_peers`).
#![cfg(target_os = "linux")]

use std::net::TcpListener;

use teseladb_io::secure::Channel;

#[path = "common/tls.rs"]
#[allow(dead_code, reason = "the helpers the other suites use too")]
mod tls;

/// Each thread reads its own copy of the settings between nodes (023 FR-023): a replacement on
/// one thread serves what that thread does next, in every clone, and leaves the others with the
/// settings they had; nothing is shared between threads but the settings first given.
#[test]
fn a_replacement_reaches_this_thread_and_every_clone_on_it() {
    let the_ca = tls::ca(1);
    let peer = tls::peer_tls(&the_ca, 2);
    let clone = peer.clone();
    let key = |t: &teseladb_io::secure::PeerTls| t.server().identity.key.public_key();
    assert_eq!(key(&peer), tls::ec(2).public_key());
    assert_eq!(format!("{peer:?}"), "PeerTls");
    peer.replace_here(&tls::peer_tls(&the_ca, 3));
    assert_eq!(key(&peer), tls::ec(3).public_key());
    assert_eq!(key(&clone), tls::ec(3).public_key(), "the clones share the copy");
    let id = clone.client().identity.clone().expect("a client identity");
    assert_eq!(id.key.public_key(), tls::ec(3).public_key());
    // Another thread never saw the replacement.
    let other = clone.clone();
    let there = std::thread::spawn(move || key(&other)).join().unwrap();
    assert_eq!(there, tls::ec(2).public_key());
    // Settings made apart do not share copies.
    let apart = tls::peer_tls(&the_ca, 4);
    assert_eq!(key(&apart), tls::ec(4).public_key());
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
