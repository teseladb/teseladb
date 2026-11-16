//! Tests of the RPC value types and the LZ4 frame compressor without a connection: error
//! kinds and messages, optional values, cancellation handles, tuples, compression round
//! trips with head space, short, empty and corrupt frames, the periodic release of the
//! reusable buffers, and negotiation through the LZ4 and multi-algorithm factories.
#![cfg(target_os = "linux")]

use std::pin::pin;

use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::rpc::lz4_compressor::{Lz4Compressor, Lz4CompressorFactory};
use teseladb::engine::rpc::multi_algo_compressor_factory::MultiAlgoCompressorFactory;
use teseladb::engine::rpc::rpc_types::{
    Bufs, Cancellable, Compressor, CompressorFactory, Error, ErrorKind, Optional, RcvBuf, SndBuf,
    Tuple, canceled_error, closed_error, remote_verb_error, rpc_protocol_error, stream_closed,
    timeout_error, unknown_exception_error, unknown_verb_error,
};

fn bytes_of(bufs: &Bufs) -> Vec<u8> {
    match bufs {
        Bufs::One(b) => b.as_slice().to_vec(),
        Bufs::Many(v) => v.iter().flat_map(|b| b.as_slice().to_vec()).collect(),
    }
}

#[test]
fn rpc_errors_carry_their_kind_and_message() {
    let e = Error::new("custom");
    assert_eq!(e.what(), "custom");
    assert_eq!(e.to_string(), "custom");
    assert_eq!(e.kind(), ErrorKind::Error);
    assert_eq!(unknown_verb_error(42).kind(), ErrorKind::UnknownVerb(42));
    assert_eq!(unknown_verb_error(42).what(), "unknown verb");
    assert_eq!(unknown_exception_error().kind(), ErrorKind::UnknownException);
    assert_eq!(unknown_exception_error().what(), "unknown exception");
    assert_eq!(rpc_protocol_error().kind(), ErrorKind::RpcProtocol);
    assert_eq!(rpc_protocol_error().what(), "rpc protocol exception");
    assert_eq!(closed_error().kind(), ErrorKind::Closed);
    assert_eq!(timeout_error().kind(), ErrorKind::Timeout);
    assert_eq!(canceled_error().kind(), ErrorKind::Canceled);
    assert_eq!(stream_closed().kind(), ErrorKind::StreamClosed);
    assert_eq!(remote_verb_error("remote").what(), "remote");
}

#[test]
fn optionals_and_tuples_wrap_values() {
    let mut o = Optional(Some(3));
    assert_eq!(*o, Some(3));
    *o = Some(4);
    assert_eq!(o.0, Some(4));
    assert_eq!(o.to_string(), "optional(4)");
    let none: Optional<i32> = Optional(None);
    assert_eq!(none.to_string(), "none");
    assert!(none.is_none());

    let t: Tuple<(i32, &str)> = (1, "a").into();
    assert_eq!(t.0, (1, "a"));
}

#[test]
fn cancellation_handles_assign_without_pending_calls() {
    let mut a = pin!(Cancellable::default());
    let mut b = pin!(Cancellable::default());
    a.as_mut().assign_from(b.as_mut());
    a.as_mut().cancel();
    b.as_mut().cancel();
}

#[test]
fn lz4_frames_round_trip_with_head_space() {
    let text: Vec<u8> = b"hello hello hello hello ".repeat(40);
    let mut c = Lz4Compressor;
    assert_eq!(c.name(), "LZ4");
    let snd = SndBuf::from_buffer(TemporaryBuffer::copy_from(&text));
    let compressed = c.compress(8, snd).unwrap();
    let mut frame = bytes_of(&compressed.bufs);
    assert_eq!(frame.len(), compressed.size as usize);
    assert!(frame.len() < text.len(), "repetitive text compresses");
    frame.drain(..8);
    let back = c.decompress(RcvBuf::from_buffer(TemporaryBuffer::copy_from(&frame))).unwrap();
    assert_eq!(back.size as usize, text.len());
    assert_eq!(bytes_of(&back.bufs), text);

    // A frame split in several buffers decompresses the same.
    let (a, b) = frame.split_at(5);
    let many = RcvBuf::from_buffers(
        vec![TemporaryBuffer::copy_from(a), TemporaryBuffer::copy_from(b)],
        frame.len(),
    );
    assert_eq!(bytes_of(&c.decompress(many).unwrap().bufs), text);

    // Data in several send buffers compresses as one.
    let parts = SndBuf::from_buffers(
        vec![TemporaryBuffer::copy_from(&text[..10]), TemporaryBuffer::copy_from(&text[10..])],
        text.len(),
    );
    let compressed = c.compress(0, parts).unwrap();
    let frame = bytes_of(&compressed.bufs);
    let back = c.decompress(RcvBuf::from_buffer(TemporaryBuffer::copy_from(&frame))).unwrap();
    assert_eq!(bytes_of(&back.bufs), text);
}

#[test]
fn lz4_rejects_short_empty_and_corrupt_frames() {
    let mut c = Lz4Compressor;
    // Shorter than the size prefix: an empty buffer.
    let r = c.decompress(RcvBuf::from_buffer(TemporaryBuffer::copy_from(b"abc"))).unwrap();
    assert_eq!(r.size, 0);
    // A zero decompressed size.
    assert!(
        c.decompress(RcvBuf::from_buffer(TemporaryBuffer::copy_from(&[0, 0, 0, 0, 1]))).is_err()
    );
    // A body that is not LZ4 data.
    let mut bad = 100u32.to_le_bytes().to_vec();
    bad.extend_from_slice(&[0xFF; 16]);
    assert!(c.decompress(RcvBuf::from_buffer(TemporaryBuffer::copy_from(&bad))).is_err());
}

#[test]
fn reusable_buffers_are_released_periodically() {
    let mut c = Lz4Compressor;
    // More uses than the release trigger: the buffers are dropped and grown again.
    for i in 0..100_010u32 {
        let snd = SndBuf::from_buffer(TemporaryBuffer::copy_from(&i.to_le_bytes()));
        let out = c.compress(0, snd).unwrap();
        assert!(out.size > 4);
    }
}

#[test]
fn factories_negotiate_the_algorithm() {
    let f = Lz4CompressorFactory;
    assert_eq!(f.supported().as_str(), "LZ4");
    assert!(f.negotiate(Sstring::from("LZ4"), true).is_some());
    assert!(f.negotiate(Sstring::from("ZSTD"), false).is_none());

    let multi = MultiAlgoCompressorFactory::from_one(&Lz4CompressorFactory);
    assert_eq!(multi.supported().as_str(), "LZ4");
    let server = multi.negotiate(Sstring::from("ZSTD,LZ4"), true).expect("LZ4 is shared");
    assert_eq!(server.name(), "LZ4");
    assert!(multi.negotiate(Sstring::from("LZ4"), false).is_some());
    assert!(multi.negotiate(Sstring::from("ZSTD,SNAPPY"), true).is_none());
    assert!(multi.negotiate(Sstring::from("ZSTD"), false).is_none());
}
