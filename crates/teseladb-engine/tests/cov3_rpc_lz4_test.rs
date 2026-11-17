//! The LZ4_FRAGMENTED RPC compressor beyond rpc_test's round trips (the original's
//! rpc/lz4_fragmented_compressor.cc): its factory's name and negotiation, an input too
//! short for a header, and frames whose compressed bytes are corrupt (a single chunk, and
//! the last chunk of a long message), which fail to decompress. Also a client's auxiliary
//! object read as another type (rpc.hh, `client_info::retrieve_auxiliary`).
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::rpc::lz4_fragmented_compressor::{
    Lz4FragmentedCompressor, Lz4FragmentedCompressorFactory,
};

use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::rpc::rpc_types::{
    Bufs, ClientInfo, Compressor, CompressorFactory, ConnectionId, RcvBuf, SndBuf,
};

/// The bytes of a compressed frame.
fn bytes(b: &SndBuf) -> Vec<u8> {
    match &b.bufs {
        Bufs::One(t) => t.as_slice().to_vec(),
        Bufs::Many(v) => v.iter().flat_map(|t| t.as_slice().to_vec()).collect(),
    }
}

fn rcv(v: &[u8]) -> RcvBuf {
    RcvBuf::from_buffer(TemporaryBuffer::copy_from(v))
}

#[test]
fn an_auxiliary_object_of_another_type_is_a_bad_cast() {
    let mut ci = ClientInfo {
        addr: SocketAddress::default(),
        server: std::ptr::null_mut(),
        conn_id: ConnectionId::make_id(1, 0),
        user_data: HashMap::new(),
    };
    let key = Sstring::from("k");
    ci.attach_auxiliary(&key, 5u32);
    // `attach` keeps the first object of a key.
    ci.attach_auxiliary(&key, 6u32);
    assert_eq!(*ci.retrieve_auxiliary::<u32>(&key).expect("a u32"), 5);
    let e = ci.retrieve_auxiliary::<String>(&key).expect_err("not a String");
    assert!(e.to_string().contains("bad_any_cast"), "{e}");
}

#[test]
fn the_factory_negotiates_its_own_name_only() {
    let f = Lz4FragmentedCompressorFactory;
    assert_eq!(f.supported().as_str(), "LZ4_FRAGMENTED");
    let c = f.negotiate(Sstring::from("LZ4_FRAGMENTED"), true).expect("negotiated");
    assert_eq!(c.name().as_str(), "LZ4_FRAGMENTED");
    assert!(f.negotiate(Sstring::from("LZ4"), false).is_none());
}

#[test]
fn corrupt_frames_fail_to_decompress() {
    let mut c = Lz4FragmentedCompressor;
    // Shorter than a chunk header: an empty message.
    assert_eq!(c.decompress(rcv(&[1, 2])).expect("short input").size, 0);

    // A small message: one chunk; its compressed bytes overwritten.
    let small: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
    let mut frame = bytes(
        &c.compress(0, SndBuf::from_buffer(TemporaryBuffer::copy_from(&small)))
            .expect("compressed"),
    );
    assert_eq!(c.decompress(rcv(&frame)).expect("round trip").size, 1000);
    for b in frame.iter_mut().skip(4) {
        *b = 0xff;
    }
    assert!(c.decompress(rcv(&frame)).is_err());

    // A message of several chunks: its last chunk overwritten.
    let big: Vec<u8> =
        (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mut frame = bytes(
        &c.compress(0, SndBuf::from_buffer(TemporaryBuffer::copy_from(&big))).expect("compressed"),
    );
    assert_eq!(c.decompress(rcv(&frame)).expect("round trip").size, 200_000);
    let n = frame.len();
    for b in frame[n - 64..].iter_mut() {
        *b = 0xff;
    }
    assert!(c.decompress(rcv(&frame)).is_err());
}
