//! A forwarded request or answer carries its sender's HLC (005 FR-003, protocol 7): the receiver's
//! clock moves past it before the request runs, so a write a node forwards to a range's leader
//! commits above every mark the node already gave its clients. Before, a change stream opened on a
//! node that does not lead the range answered a resume token from the node's clock, the insert
//! that followed committed at the leader's lower clock, and the stream resumed past it (PyRef's
//! `change-streams-resume-allowlist`, e2e of 04-11-2026). To a node of an older protocol the frame
//! goes as before, without the clock.

use teseladb_io::peer::{
    CLOCKED_FORWARD_VERSION, FORWARD_CHUNK_VERSION, HEADER_LEN, PROTOCOL_VERSION, PeerMsg,
    RAFT_CHUNK_KIND, decode_frame, encode_frame, encode_frame_for,
};
use teseladb_keys::Hlc;
use teseladb_types::Bytes;

const CLOCK: Hlc = Hlc { physical: 1_793_817_549_000, logical: 415_236_097 };
const CLOCKED_FORWARD_KIND: u8 = 0x35;
const CLOCKED_FORWARD_RESP_KIND: u8 = 0x36;

fn messages(body: Bytes) -> [PeerMsg; 2] {
    [
        PeerMsg::Forward { req: 8, deadline_ms: 5_000, hlc: Some(CLOCK), body: body.clone() },
        PeerMsg::ForwardResp { req: 8, hlc: Some(CLOCK), body },
    ]
}

/// Decodes every message of `stream`.
fn decode_all(stream: &[u8]) -> Vec<PeerMsg> {
    let mut at = 0;
    let mut got = Vec::new();
    while let Some((m, n)) = decode_frame(&stream[at..]).unwrap() {
        got.push(m);
        at += n;
    }
    assert_eq!(at, stream.len(), "nothing is left over");
    got
}

#[test]
fn protocol_7_carries_the_clock_of_a_forward() {
    assert_eq!(PROTOCOL_VERSION, 7);
    assert_eq!(CLOCKED_FORWARD_VERSION, 7);
}

#[test]
fn a_forward_with_its_senders_clock_comes_back_with_it() {
    let kinds = [CLOCKED_FORWARD_KIND, CLOCKED_FORWARD_RESP_KIND];
    for (msg, kind) in messages(Bytes::from("request")).into_iter().zip(kinds) {
        let mut out = Vec::new();
        encode_frame_for(&msg, false, CLOCKED_FORWARD_VERSION, &mut out).unwrap();
        assert_eq!(out[4], kind);
        let mut plain = Vec::new();
        encode_frame(&msg, false, &mut plain).unwrap();
        assert_eq!(out, plain);
        assert_eq!(decode_all(&out), vec![msg]);
    }
}

#[test]
fn a_node_of_an_older_protocol_gets_the_forward_without_the_clock() {
    for msg in messages(Bytes::from("request")) {
        let mut out = Vec::new();
        encode_frame_for(&msg, false, CLOCKED_FORWARD_VERSION - 1, &mut out).unwrap();
        let got = decode_all(&out);
        let bare = match msg {
            PeerMsg::Forward { req, deadline_ms, body, .. } => {
                assert_eq!(out[4], 0x30);
                PeerMsg::Forward { req, deadline_ms, hlc: None, body }
            }
            PeerMsg::ForwardResp { req, body, .. } => {
                assert_eq!(out[4], 0x31);
                PeerMsg::ForwardResp { req, hlc: None, body }
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, vec![bare]);
    }
}

#[test]
fn a_forward_with_its_clock_larger_than_a_frame_travels_in_pieces() {
    let big = Bytes::from_vec(vec![7; 18 << 20]);
    for msg in messages(big) {
        let mut out = Vec::new();
        encode_frame_for(&msg, false, CLOCKED_FORWARD_VERSION, &mut out).unwrap();
        assert_eq!(out[4], RAFT_CHUNK_KIND);
        assert_eq!(out[HEADER_LEN], FORWARD_CHUNK_VERSION);
        assert_eq!(decode_all(&out), vec![msg]);
    }
}

#[test]
fn a_clocked_forward_cut_short_is_rejected() {
    let msg = PeerMsg::ForwardResp { req: 1, hlc: Some(CLOCK), body: Bytes::new() };
    let mut out = Vec::new();
    encode_frame(&msg, false, &mut out).unwrap();
    // Keep the request number and part of the clock, with the header's length matching.
    let cut = HEADER_LEN + 8 + 6;
    let mut short = out[..cut].to_vec();
    short[..4].copy_from_slice(&((cut - 4) as u32).to_le_bytes());
    assert!(decode_frame(&short).is_err());
}
