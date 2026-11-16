//! The blocking side of a peer connection's channel (013 FR-006): in clear it passes the bytes as
//! they are. TLS between nodes, enrollments and their refusals run on the engine
//! (`engine_peers`).
#![cfg(target_os = "linux")]

use std::net::TcpListener;

use teseladb_io::secure::Channel;

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
